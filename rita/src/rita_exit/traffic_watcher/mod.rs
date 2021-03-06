//! Traffic watcher monitors system traffic by interfacing with KernelInterface to create and check
//! iptables and ipset counters on each per hop tunnel (the WireGuard tunnel between two devices). These counts
//! are then stored and used to compute amounts for bills.
//!
//! This is the exit specific billing code used to determine how exits should be compensted. Which is
//! different in that mesh nodes are paid by forwarding traffic, but exits have to return traffic and
//! must get paid for doing so.

use actix::prelude::*;

use althea_kernel_interface::wg_iface_counter::WgUsage;
use althea_kernel_interface::KI;

use althea_types::Identity;

use babel_monitor::Babel;

use rita_common::debt_keeper;
use rita_common::debt_keeper::DebtKeeper;

use num256::Int256;

use std::collections::HashMap;
use std::io::{Read, Write};
use std::net::{IpAddr, SocketAddr, TcpStream};

use ipnetwork::IpNetwork;

use settings::{RitaCommonSettings, RitaExitSettings};
use SETTING;

use failure::Error;

pub struct TrafficWatcher {
    last_seen_bytes: HashMap<String, WgUsage>,
}

impl Actor for TrafficWatcher {
    type Context = Context<Self>;
}
impl Supervised for TrafficWatcher {}
impl SystemService for TrafficWatcher {
    fn service_started(&mut self, _ctx: &mut Context<Self>) {
        match KI.setup_wg_if_named("wg_exit") {
            Err(e) => warn!("exit setup returned {}", e),
            _ => {}
        }
        KI.setup_nat(&SETTING.get_network().external_nic.clone().unwrap())
            .unwrap();

        info!("Traffic Watcher started");
    }
}
impl Default for TrafficWatcher {
    fn default() -> TrafficWatcher {
        TrafficWatcher {
            last_seen_bytes: HashMap::new(),
        }
    }
}

pub struct Watch(pub Vec<Identity>);

impl Message for Watch {
    type Result = Result<(), Error>;
}

impl Handler<Watch> for TrafficWatcher {
    type Result = Result<(), Error>;

    fn handle(&mut self, msg: Watch, _: &mut Context<Self>) -> Self::Result {
        let stream = TcpStream::connect::<SocketAddr>(
            format!("[::1]:{}", SETTING.get_network().babel_port).parse()?,
        )?;

        watch(&mut self.last_seen_bytes, Babel::new(stream), msg.0)
    }
}

/// This traffic watcher watches how much traffic each we send and receive from each client.
pub fn watch<T: Read + Write>(
    usage_history: &mut HashMap<String, WgUsage>,
    mut babel: Babel<T>,
    clients: Vec<Identity>,
) -> Result<(), Error> {
    babel.start_connection()?;

    trace!("Getting routes");
    let routes = babel.parse_routes()?;
    info!("Got routes: {:?}", routes);

    let mut identities: HashMap<String, Identity> = HashMap::new();
    let mut id_from_ip: HashMap<IpAddr, Identity> = HashMap::new();
    let our_id = Identity {
        mesh_ip: match SETTING.get_network().mesh_ip {
            Some(ip) => ip.clone(),
            None => bail!("No mesh ip configured yet!"),
        },
        eth_address: SETTING.get_payment().eth_address.clone(),
        wg_public_key: SETTING.get_network().wg_public_key.clone(),
    };
    id_from_ip.insert(SETTING.get_network().mesh_ip.unwrap(), our_id.clone());

    for ident in &clients {
        identities.insert(ident.wg_public_key.clone(), ident.clone());
        id_from_ip.insert(ident.mesh_ip, ident.clone());
    }

    // insert ourselves as a destination, don't think this is actually needed
    let mut destinations = HashMap::new();
    destinations.insert(
        our_id.wg_public_key,
        Int256::from(babel.get_local_fee().unwrap()),
    );

    for route in &routes {
        // Only ip6
        if let IpNetwork::V6(ref ip) = route.prefix {
            // Only host addresses and installed routes
            if ip.prefix() == 128 && route.installed {
                match id_from_ip.get(&IpAddr::V6(ip.ip())) {
                    Some(id) => {
                        destinations.insert(id.wg_public_key.clone(), Int256::from(route.price));
                    }
                    None => warn!("Can't find destinatoin for client {:?}", ip.ip()),
                }
            }
        }
    }

    let counters = match KI.read_wg_counters("wg_exit") {
        Ok(res) => res,
        Err(e) => {
            warn!(
                "Error getting input counters {:?} traffic has gone unaccounted!",
                e
            );
            return Err(e);
        }
    };

    trace!("exit counters: {:?}", counters);

    let mut total_in: u64 = 0;
    for entry in counters.iter() {
        let input = entry.1;
        total_in += input.download;
    }
    info!("Total Exit input of {} bytes this round", total_in);
    let mut total_out: u64 = 0;
    for entry in counters.iter() {
        let output = entry.1;
        total_out += output.upload;
    }
    info!("Total Exit output of {} bytes this round", total_out);

    let mut debts = HashMap::new();

    // Setup the debts table
    for (_, ident) in identities.clone() {
        debts.insert(ident, Int256::from(0));
    }

    let price = SETTING.get_exit_network().exit_price;

    // setup bandwidth history
    for (wg_key, bytes) in counters.clone() {
        match usage_history.get(&wg_key) {
            Some(_) => (),
            None => {
                trace!(
                    "We have not seen {:?} before, starting counter off at {:?}",
                    wg_key,
                    bytes
                );
                usage_history.insert(wg_key, bytes);
            }
        }
    }

    // accounting for 'input'
    for (wg_key, bytes) in counters.clone() {
        let state = (
            identities.get(&wg_key),
            destinations.get(&wg_key),
            usage_history.get_mut(&wg_key),
        );
        match state {
            (Some(id), Some(_dest), Some(history)) => match debts.get_mut(&id) {
                Some(debt) => {
                    // tunnel has been reset somehow, reset usage
                    if history.download > bytes.download {
                        history.download = 0;
                    }
                    *debt -= price * (bytes.download - history.download);
                    // update history so that we know what was used from previous cycles
                    history.download = bytes.download;
                }
                // debts is generated from identities, this should be impossible
                None => warn!("No debts entry for input entry id {:?}", id),
            },
            (Some(id), Some(_dest), None) => warn!("Entry for {:?} should have been created", id),
            // this can be caused by a peer that has not yet formed a babel route
            (Some(id), None, _) => warn!("We have an id {:?} but not destination", id),
            // if we have a babel route we should have a peer it's possible we have a mesh client sneaking in?
            (None, Some(dest), _) => warn!("We have a destination {:?} but no id", dest),
            // dead entry?
            (None, None, _) => warn!("We have no id or dest for an input counter on {:?}", wg_key),
        }
    }

    trace!("Collated input exit debts: {:?}", debts);

    // accounting for 'output'
    for (wg_key, bytes) in counters {
        let state = (
            identities.get(&wg_key),
            destinations.get(&wg_key),
            usage_history.get_mut(&wg_key),
        );
        match state {
            (Some(id), Some(dest), Some(history)) => match debts.get_mut(&id) {
                Some(debt) => {
                    // tunnel has been reset somehow, reset usage
                    if history.upload > bytes.upload {
                        history.upload = 0;
                    }
                    *debt -= (dest.clone() + price) * (bytes.upload - history.upload);
                    history.upload = bytes.upload;
                }
                // debts is generated from identities, this should be impossible
                None => warn!("No debts entry for input entry id {:?}", id),
            },
            (Some(id), Some(_dest), None) => warn!("Entry for {:?} should have been created", id),
            // this can be caused by a peer that has not yet formed a babel route
            (Some(id), None, _) => warn!("We have an id {:?} but not destination", id),
            // if we have a babel route we should have a peer it's possible we have a mesh client sneaking in?
            (None, Some(dest), _) => warn!("We have a destination {:?} but no id", dest),
            // dead entry?
            (None, None, _) => warn!("We have no id or dest for an input counter on {:?}", wg_key),
        }
    }

    trace!("Collated total exit debts: {:?}", debts);

    info!("Computed exit debts for {:?} clients", debts.len());
    let mut total_income = Int256::zero();
    for entry in debts.iter() {
        let income = entry.1;
        total_income += income;
    }
    info!("Total exit income of {:?} Wei this round", total_income);

    match KI.get_wg_exit_clients_online() {
        Ok(users) => info!("Total of {} users online", users),
        Err(e) => warn!("Getting clients failed with {:?}", e),
    }

    for (from, amount) in debts {
        let update = debt_keeper::TrafficUpdate {
            from: from.clone(),
            amount,
        };

        DebtKeeper::from_registry().do_send(update);
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    extern crate env_logger;

    use super::*;

    #[test]
    #[ignore]
    fn debug_babel_socket_client() {
        env_logger::init();
        let bm_stream = TcpStream::connect::<SocketAddr>("[::1]:9001".parse().unwrap()).unwrap();
        watch(&mut HashMap::new(), Babel::new(bm_stream), Vec::new()).unwrap();
    }
}
