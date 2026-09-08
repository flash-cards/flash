//! Card content validation.

pub const MAX_SIDE_LEN: usize = 10_000;

/// Validated front/back text for a card.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CardText {
    pub front: String,
    pub back: String,
}

/// Trims both sides and rejects empty or oversized content.
pub fn validate_card_text(front: &str, back: &str) -> Result<CardText, String> {
    let front = front.trim();
    let back = back.trim();
    if front.is_empty() {
        return Err("card front is empty".into());
    }
    if back.is_empty() {
        return Err("card back is empty".into());
    }
    if front.len() > MAX_SIDE_LEN || back.len() > MAX_SIDE_LEN {
        return Err(format!("card side exceeds {MAX_SIDE_LEN} bytes"));
    }
    Ok(CardText {
        front: front.to_string(),
        back: back.to_string(),
    })
}
