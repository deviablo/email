// SPDX-License-Identifier: GPL-3.0-or-later

fn main() {
    println!("cargo:rerun-if-env-changed=E_MAIL_LOCALEDIR");
    println!("cargo:rerun-if-env-changed=E_MAIL_PACKAGE");
}
