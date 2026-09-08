//! Stats: everything the web page shows, precomputed by `Services::stats`
//! (tiles, the year heatmap with its levels and month spans, the 30-day
//! bars, the next seven days) so the app draws and never counts.

use axum::extract::State;
use axum::routing::get;
use axum::{Json, Router};
use serde::Serialize;

use super::{call, ApiResult, BearerUser};
use crate::service::{self, now_ms};
use crate::state::AppState;

pub fn router() -> Router<AppState> {
    Router::new().route("/stats", get(stats))
}

#[derive(Serialize)]
struct DayCount {
    label: String,
    count: u32,
    /// Bar height as a percentage of the busiest day.
    pct: u32,
}

impl From<service::DayCount> for DayCount {
    fn from(d: service::DayCount) -> Self {
        DayCount {
            label: d.label,
            count: d.count,
            pct: d.pct,
        }
    }
}

#[derive(Serialize)]
struct HeatCell {
    count: u32,
    /// 0 = empty, 1-4 = intensity relative to the busiest day.
    level: u8,
    /// MM/DD.
    label: String,
    /// After today: `count` is cards due, not reviews done.
    future: bool,
    today: bool,
}

#[derive(Serialize)]
struct MonthSpan {
    label: String,
    weeks: u32,
}

#[derive(Serialize)]
struct Stats {
    reviews_30d: u32,
    /// Null until there are mature reviews to measure.
    retention_pct: Option<u32>,
    streak: u32,
    longest_streak: u32,
    daily_avg: u32,
    days_learned_pct: u32,
    total_reviews: u32,
    active_cards: u32,
    mature_cards: u32,
    /// The last 30 days, oldest first.
    days: Vec<DayCount>,
    /// The next 7 days.
    upcoming: Vec<DayCount>,
    /// 53 week columns of 7 cells (Sunday first), oldest week first.
    heat_weeks: Vec<Vec<HeatCell>>,
    heat_months: Vec<MonthSpan>,
}

async fn stats(State(state): State<AppState>, user: BearerUser) -> ApiResult<Stats> {
    let heavy = state.heavy(user.id)?;
    let s = call(state.services.clone(), move |s| s.stats(&heavy, now_ms())).await?;
    Ok(Json(Stats {
        reviews_30d: s.reviews_30d,
        retention_pct: s.retention_pct,
        streak: s.streak,
        longest_streak: s.longest_streak,
        daily_avg: s.daily_avg,
        days_learned_pct: s.days_learned_pct,
        total_reviews: s.total_reviews,
        active_cards: s.active_cards,
        mature_cards: s.mature_cards,
        days: s.days.into_iter().map(Into::into).collect(),
        upcoming: s.upcoming.into_iter().map(Into::into).collect(),
        heat_weeks: s
            .heat_weeks
            .into_iter()
            .map(|week| {
                week.into_iter()
                    .map(|c| HeatCell {
                        count: c.count,
                        level: c.level,
                        label: c.label,
                        future: c.future,
                        today: c.today,
                    })
                    .collect()
            })
            .collect(),
        heat_months: s
            .heat_months
            .into_iter()
            .map(|m| MonthSpan {
                label: m.label,
                weeks: m.weeks,
            })
            .collect(),
    }))
}
