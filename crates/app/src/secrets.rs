// SPDX-License-Identifier: GPL-3.0-or-later

//! Passwords live only in the system keyring (Secret Service, or the secret
//! portal inside Flatpak). They are never written to files or logs.

use crate::config::{APP_ID, LEGACY_APP_ID};

fn attributes(email: &str) -> [(&'static str, String); 2] {
    attributes_for(APP_ID, email)
}

fn attributes_for(app_id: &str, email: &str) -> [(&'static str, String); 2] {
    [
        ("application", app_id.to_owned()),
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
    if let Some(item) = items.first() {
        item.unlock().await?;
        let secret = item.secret().await?;
        return Ok(Some(String::from_utf8_lossy(&secret).into_owned()));
    }
    // Passwords saved before the App ID changed are moved over once.
    let legacy = attributes_for(LEGACY_APP_ID, &email);
    let items = keyring.search_items(&legacy).await?;
    let Some(item) = items.first() else {
        return Ok(None);
    };
    item.unlock().await?;
    let password = String::from_utf8_lossy(&item.secret().await?).into_owned();
    keyring
        .create_item(
            &format!("E-Mail: {email}"),
            &attributes(&email),
            password.clone(),
            true,
        )
        .await?;
    keyring.delete(&legacy).await?;
    Ok(Some(password))
}

pub async fn delete_password(email: String) -> anyhow::Result<()> {
    let keyring = oo7::Keyring::new().await?;
    keyring.unlock().await?;
    keyring.delete(&attributes(&email)).await?;
    keyring
        .delete(&attributes_for(LEGACY_APP_ID, &email))
        .await?;
    Ok(())
}
