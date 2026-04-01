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
         \u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\n\n",
    );

    if wallets.is_empty() {
        text.push_str(
            "\u{26a0}\u{fe0f} <b>Belum ada wallet</b>\n\n\
             Buat wallet baru atau import private key\n\
             untuk mulai trading.\n\n\
             \u{1f447} <i>Pilih aksi:</i>",
        );
    } else {
        for w in wallets {
            let active_badge = if w.is_active { " \u{2705}" } else { "" };
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
                "{} <b>{}</b>{}\n\
                 \u{1f4cb} <code>{}</code>\n\
                 \u{1f4b0} {:.4} SOL\n\n",
                if w.is_active { "\u{1f7e2}" } else { "\u{26ab}" },
                label, active_badge, short_addr, w.balance_sol,
            ));
        }
        text.push_str("\u{1f447} <i>Pilih aksi:</i>");
    }

    let mut rows: Vec<Vec<InlineKeyboardButton>> = Vec::new();

    // Action buttons
    rows.push(vec![
        InlineKeyboardButton::callback("\u{2728} Generate Baru", "wallet:generate"),
        InlineKeyboardButton::callback("\u{1f4e5} Import Key", "wallet:import"),
    ]);

    if !wallets.is_empty() {
        rows.push(vec![
            InlineKeyboardButton::callback("\u{1f504} Switch Wallet", "wallet:switch"),
            InlineKeyboardButton::callback("\u{1f4cb} Deposit Address", "wallet:show_address"),
        ]);
        rows.push(vec![
            InlineKeyboardButton::callback("\u{1f4b8} Withdraw SOL", "wallet:withdraw"),
            InlineKeyboardButton::callback("\u{1f5d1}\u{fe0f} Hapus Wallet", "wallet:delete"),
        ]);
    }

    // Back
    rows.push(vec![InlineKeyboardButton::callback(
        "\u{2b05}\u{fe0f} Menu Utama",
        "menu",
    )]);

    let kb = InlineKeyboardMarkup::new(rows);
    (text, kb)
}
