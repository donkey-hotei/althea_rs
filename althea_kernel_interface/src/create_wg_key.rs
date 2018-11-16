use super::KernelInterface;

use std::fs::File;
use std::io::Write;
use std::path::Path;
use std::process::{Command, Stdio};
use std::str::FromStr;

use althea_types::wg_key::WgKey;
use failure::Error;

#[derive(Debug)]
pub struct WgKeypair {
    pub public: WgKey,
    pub private: WgKey,
}

impl KernelInterface {
    pub fn create_wg_key(&self, path: &Path, private_key: &String) -> Result<(), Error> {
        trace!("Overwriting old private key file");
        let mut priv_key_file = File::create(path)?;
        write!(priv_key_file, "{}", private_key)?;
        Ok(())
    }

    pub fn create_wg_keypair(&self) -> Result<WgKeypair, Error> {
        let genkey = Command::new("wg")
            .args(&["genkey"])
            .stdout(Stdio::piped())
            .output()
            .unwrap();

        let mut pubkey = Command::new("wg")
            .args(&["pubkey"])
            .stdout(Stdio::piped())
            .stdin(Stdio::piped())
            .spawn()
            .unwrap();

        pubkey
            .stdin
            .as_mut()
            .unwrap()
            .write_all(&genkey.stdout)
            .expect("Failure generating wg keypair!");
        let output = pubkey.wait_with_output().unwrap();

        let mut privkey_str = String::from_utf8(genkey.stdout)?;
        let mut pubkey_str = String::from_utf8(output.stdout)?;

        privkey_str.truncate(44);
        pubkey_str.truncate(44);

        let private = WgKey::from_str(&privkey_str).unwrap();
        let public = WgKey::from_str(&pubkey_str).unwrap();

        Ok(WgKeypair { public, private })
    }
}

// Tested in CLU
