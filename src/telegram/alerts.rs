use teloxide::types::{InlineKeyboardButton, InlineKeyboardMarkup};

use crate::models::token::{
    CreatorHistory, HoneypotResult, LpStatus, SecurityAnalysis, TokenInfo,
};
use crate::models::trade::{Trade, TradeType};
use crate::models::Position;

/// Format a new token detection alert with security analysis and buy buttons.
pub fn format_new_token_alert(
    token: &TokenInfo,
    analysis: &SecurityAnalysis,
) -> (String, InlineKeyboardMarkup) {
    let score_emoji = if analysis.final_score >= 75 {
        "\u{1f7e2}"
    } else if analysis.final_score >= 50 {
        "\u{1f7e1}"
    } else {
        "\u{1f534}"
    };

    let mint_check = if analysis.mint_renounced { "\u{2705}" } else { "\u{274c}" };
    let freeze_check = if analysis.freeze_authority_null { "\u{2705}" } else { "\u{274c}" };
    let meta_check = if analysis.metadata_immutable { "\u{2705}" } else { "\u{274c}" };

    let lp_text = match &analysis.lp_status {
        LpStatus::Burned => "\u{1f525} Burned",
        LpStatus::Locked => "\u{1f512} Locked",
        LpStatus::NotBurned => "\u{26a0}\u{fe0f} Not Burned",
        LpStatus::Unknown => "\u{2753} Unknown",
    };

    let honeypot_text = match &analysis.honeypot_result {
        HoneypotResult::Safe { buy_tax, sell_tax } => {
            format!("\u{2705} Safe (buy: {:.1}% / sell: {:.1}%)", buy_tax, sell_tax)
        }
        HoneypotResult::HighTax { buy_tax, sell_tax } => {
            format!(
                "\u{26a0}\u{fe0f} High Tax (buy: {:.1}% / sell: {:.1}%)",
                buy_tax, sell_tax
            )
        }
        HoneypotResult::Honeypot => "\u{274c} HONEYPOT DETECTED".to_string(),
        HoneypotResult::Unknown => "\u{2753} Unknown".to_string(),
    };

    let creator_text = match &analysis.creator_history {
        CreatorHistory::Clean { tokens_created } => {
            format!("\u{2705} Clean ({} tokens)", tokens_created)
        }
        CreatorHistory::Suspicious {
            tokens_created,
            rugs,
        } => format!(
            "\u{26a0}\u{fe0f} Suspicious ({} tokens, {} rugs)",
            tokens_created, rugs
        ),
        CreatorHistory::Rugger {
            tokens_created,
            rugs,
        } => format!(
            "\u{274c} RUGGER ({} tokens, {} rugs)",
            tokens_created, rugs
        ),
        CreatorHistory::Unknown => "\u{2753} Unknown".to_string(),
    };

    let socials = &analysis.social_links;
    let social_count = socials.count();
    let social_text = format!(
        "{}/3 ({}{}{})",
        social_count,
        if socials.website.is_some() { "Web " } else { "" },
        if socials.twitter.is_some() { "X " } else { "" },
        if socials.telegram.is_some() { "TG" } else { "" },
    );

    let short_mint = format!("{}...{}", &token.mint[..6], &token.mint[token.mint.len() - 4..]);

    let text = format!(
        "\u{1f6a8} <b>NEW TOKEN DETECTED</b> \u{1f6a8}\n\
         \u{2501}\u{2501}\u{2501}\u{2501}\u{2501}\u{2501}\u{2501}\u{2501}\u{2501}\u{2501}\u{2501}\u{2501}\u{2501}\u{2501}\u{2501}\u{2501}\u{2501}\u{2501}\u{2501}\u{2501}\n\n\
         \u{1f3f7}\u{fe0f} <b>{}</b> ({})\n\
         \u{1f4e6} Source: {}\n\
         \u{1f4cb} Mint: <code>{}</code>\n\
         \u{1f4b0} Liq: {:.2} SOL (${:.0})\n\n\
         {} <b>Security Score: {}/100</b>\n\n\
         <b>Checks:</b>\n\
         {} Mint Renounced\n\
         {} Freeze Authority\n\
         {} Metadata Immutable\n\
         \u{1f4a7} LP: {}\n\
         \u{1f41d} Honeypot: {}\n\
         \u{1f464} Creator: {}\n\
         \u{1f517} Socials: {}\n\n\
         \u{23f0} Detected: {}",
        token.name,
        token.symbol,
        token.source,
        short_mint,
        token.initial_liquidity_sol,
        token.initial_liquidity_usd,
        score_emoji,
        analysis.final_score,
        mint_check,
        freeze_check,
        meta_check,
        lp_text,
        honeypot_text,
        creator_text,
        social_text,
        token.detected_at.format("%H:%M:%S UTC"),
    );

    let kb = InlineKeyboardMarkup::new(vec![
        vec![
            InlineKeyboardButton::callback(
                "\u{25aa}\u{fe0f} Buy 0.1 SOL",
                format!("buy:0.1:{}", token.mint),
            ),
            InlineKeyboardButton::callback(
                "\u{25ab}\u{fe0f} Buy 0.5 SOL",
                format!("buy:0.5:{}", token.mint),
            ),
            InlineKeyboardButton::callback(
                "\u{1f7e1} Buy 1 SOL",
                format!("buy:1:{}", token.mint),
            ),
        ],
        vec![
            InlineKeyboardButton::callback(
                "\u{1f50d} Details",
                format!("token_detail:{}", token.mint),
            ),
            InlineKeyboardButton::callback("\u{274c} Dismiss", "cancel"),
        ],
    ]);

    (text, kb)
}

/// Format a trade execution notification.
pub fn format_trade_alert(trade: &Trade) -> String {
    let type_emoji = match trade.trade_type {
        TradeType::Buy => "\u{1f7e2} BUY",
        TradeType::Sell => "\u{1f534} SELL",
    };

    let tx_link = match &trade.tx_signature {
        Some(sig) => format!(
            "<a href=\"https://solscan.io/tx/{}\">View on Solscan</a>",
            sig
        ),
        None => "Pending...".to_string(),
    };

    let pnl_text = match trade.pnl_sol {
        Some(pnl) => {
            let sign = if pnl >= 0.0 { "+" } else { "" };
            format!("\n\u{1f4ca} PnL: <b>{}{:.4} SOL</b>", sign, pnl)
        }
        None => String::new(),
    };

    format!(
        "\u{1f4e3} <b>Trade Executed</b>\n\
         \u{2501}\u{2501}\u{2501}\u{2501}\u{2501}\u{2501}\u{2501}\u{2501}\u{2501}\u{2501}\u{2501}\u{2501}\u{2501}\u{2501}\u{2501}\u{2501}\u{2501}\u{2501}\u{2501}\u{2501}\n\n\
         {} <b>{}</b>\n\
         \u{1f4b0} Amount: {:.4} SOL\n\
         \u{1f4b2} Price: {:.10}\n\
         \u{1f4e6} Tokens: {:.2}{}\n\n\
         \u{1f517} {}",
        type_emoji,
        trade.token_symbol,
        trade.amount_sol,
        trade.price_per_token,
        trade.amount_tokens,
        pnl_text,
        tx_link,
    )
}

/// Format a TP/SL hit notification.
pub fn format_pnl_alert(position: &Position, action: &str) -> String {
    let pnl_emoji = if position.pnl_pct >= 0.0 {
        "\u{1f4b9}"
    } else {
        "\u{1f4c9}"
    };
    let pnl_sign = if position.pnl_pct >= 0.0 { "+" } else { "" };

    let action_emoji = match action {
        "TP" => "\u{2705} TAKE PROFIT",
        "SL" => "\u{1f6d1} STOP LOSS",
        "TRAILING" => "\u{1f4c9} TRAILING STOP",
        _ => action,
    };

    format!(
        "\u{1f514} <b>{}</b>\n\
         \u{2501}\u{2501}\u{2501}\u{2501}\u{2501}\u{2501}\u{2501}\u{2501}\u{2501}\u{2501}\u{2501}\u{2501}\u{2501}\u{2501}\u{2501}\u{2501}\u{2501}\u{2501}\u{2501}\u{2501}\n\n\
         \u{1f3f7}\u{fe0f} <b>{}</b>\n\
         \u{1f4b5} Entry: {:.8} SOL\n\
         \u{1f4b2} Exit: {:.8} SOL\n\
         {} PnL: <b>{}{:.2}%</b> ({}{:.4} SOL)\n\
         \u{1f4b0} Amount: {:.4} SOL\n\n\
         <i>Posisi ditutup otomatis.</i>",
        action_emoji,
        position.token_symbol,
        position.entry_price_sol,
        position.current_price_sol,
        pnl_emoji,
        pnl_sign,
        position.pnl_pct,
        pnl_sign,
        position.pnl_sol,
        position.entry_amount_sol,
    )
}
