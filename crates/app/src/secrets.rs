// SPDX-License-Identifier: GPL-3.0-or-later

//! Passwords live only in the system keyring (Secret Service, or the secret
//! portal inside Flatpak). They are never written to files or logs.

use crate::config::APP_ID;

fn attributes(email: &str) -> [(&'static str, String); 2] {
    [
        ("application", APP_ID.to_owned()),
        ("account", email.to_owned()),
    ]
}

pub async fn save_password(email: String, password: String) -> anyhow::Result<()> {
    let keyring = oo7::Keyring::new().await?;
    keyring.unlock().await?;
    keyring
        .create_item(
            &format!("E-Mail: {email}"),
            &attributes(&email),
            password,
            true,
        )
        .await?;
    Ok(())
}

pub async fn load_password(email: String) -> anyhow::Result<Option<String>> {
    let keyring = oo7::Keyring::new().await?;
    keyring.unlock().await?;
    let items = keyring.search_items(&attributes(&email)).await?;
    let Some(item) = items.first() else {
        return Ok(None);
    };
    item.unlock().await?;
    let secret = item.secret().await?;
    Ok(Some(String::from_utf8_lossy(&secret).into_owned()))
}

pub async fn delete_password(email: String) -> anyhow::Result<()> {
    let keyring = oo7::Keyring::new().await?;
    keyring.unlock().await?;
    keyring.delete(&attributes(&email)).await?;
    Ok(())
}
