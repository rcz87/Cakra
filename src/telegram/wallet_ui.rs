use serde::{Deserialize, Serialize};
use teloxide::types::{InlineKeyboardButton, InlineKeyboardMarkup};

/// Wallet info for display in the UI.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WalletInfo {
    pub index: u32,
    pub pubkey: String,
    pub balance_sol: f64,
    pub is_active: bool,
    pub label: Option<String>,
}

/// Build the wallet management message and keyboard.
pub fn build_wallet_message(wallets: &[WalletInfo]) -> (String, InlineKeyboardMarkup) {
    let mut text = String::from(
        "\u{1f45b} <b>Wallet Management</b>\n\
         \u{2501}\u{2501}\u{2501}\u{2501}\u{2501}\u{2501}\u{2501}\u{2501}\u{2501}\u{2501}\u{2501}\u{2501}\u{2501}\u{2501}\u{2501}\u{2501}\u{2501}\u{2501}\u{2501}\u{2501}\n\n",
    );

    if wallets.is_empty() {
        text.push_str(
            "\u{26a0}\u{fe0f} Belum ada wallet.\n\
             Generate wallet baru atau import existing wallet.\n",
        );
    } else {
        for w in wallets {
            let active = if w.is_active { " \u{2705} ACTIVE" } else { "" };
            let default_label = format!("Wallet #{}", w.index);
            let label = w
                .label
                .as_deref()
                .unwrap_or(&default_label);
            let short_addr = if w.pubkey.len() > 10 {
                format!("{}...{}", &w.pubkey[..6], &w.pubkey[w.pubkey.len() - 4..])
            } else {
                w.pubkey.clone()
            };

            text.push_str(&format!(
                "\u{1f4cd} <b>{}</b>{}\n\
                 \u{1f4cb} <code>{}</code>\n\
                 \u{1f4b0} Balance: <b>{:.4} SOL</b>\n\n",
                label, active, short_addr, w.balance_sol,
            ));
        }
    }

    text.push_str("<i>Pilih aksi di bawah:</i>");

    let mut rows: Vec<Vec<InlineKeyboardButton>> = Vec::new();

    // Action buttons
    rows.push(vec![
        InlineKeyboardButton::callback("\u{2728} Generate New", "wallet:generate"),
        InlineKeyboardButton::callback("\u{1f4e5} Import", "wallet:import"),
    ]);

    if !wallets.is_empty() {
        rows.push(vec![
            InlineKeyboardButton::callback("\u{1f504} Switch Active", "wallet:switch"),
            InlineKeyboardButton::callback("\u{1f5d1}\u{fe0f} Delete", "wallet:delete"),
        ]);
        rows.push(vec![InlineKeyboardButton::callback(
            "\u{1f4cb} Show Address (Deposit)",
            "wallet:show_address",
        )]);
    }

    // Back
    rows.push(vec![InlineKeyboardButton::callback(
        "\u{2b05}\u{fe0f} Kembali",
        "menu",
    )]);

    let kb = InlineKeyboardMarkup::new(rows);
    (text, kb)
}
