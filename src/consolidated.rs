use chrono::{DateTime, Utc};

use crate::db::MemoryRow;

pub const CONSOLIDATED_TYPES: &[&str] =
    &["trait", "value", "pattern", "preference", "mental_model"];

pub fn is_consolidated(memory_type: &str) -> bool {
    CONSOLIDATED_TYPES.contains(&memory_type)
}

const HALF_LIFE_DAYS: f32 = 180.0;

pub fn effective_score(
    confidence: f32,
    last_reinforced_at: Option<DateTime<Utc>>,
    record_time: DateTime<Utc>,
    now: DateTime<Utc>,
) -> f32 {
    let anchor = last_reinforced_at.unwrap_or(record_time);
    let days = (now - anchor).num_seconds() as f32 / 86_400.0;
    let days = days.max(0.0);
    confidence * 0.5_f32.powf(days / HALF_LIFE_DAYS)
}

pub fn is_active(row: &MemoryRow) -> bool {
    row.superseded_by.is_none() && row.invalidated_at.is_none()
}

pub fn rank(rows: Vec<MemoryRow>, now: DateTime<Utc>) -> Vec<(f32, MemoryRow)> {
    let mut scored: Vec<(f32, MemoryRow)> = rows
        .into_iter()
        .map(|row| {
            let confidence = row.confidence.unwrap_or(0.0);
            let es = effective_score(confidence, row.last_reinforced_at, row.record_time, now);
            (es, row)
        })
        .collect();

    scored.sort_by(|a, b| {
        b.0.partial_cmp(&a.0)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| {
                let a_lr = a.1.last_reinforced_at.as_ref();
                let b_lr = b.1.last_reinforced_at.as_ref();
                match (b_lr, a_lr) {
                    (Some(b_t), Some(a_t)) => b_t.cmp(a_t),
                    (Some(_), None) => std::cmp::Ordering::Less,
                    (None, Some(_)) => std::cmp::Ordering::Greater,
                    (None, None) => std::cmp::Ordering::Equal,
                }
            })
            .then_with(|| b.1.record_time.cmp(&a.1.record_time))
            .then_with(|| a.1.id.cmp(&b.1.id))
    });

    scored
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Duration;

    fn utc(year: i32, month: u32, day: u32) -> DateTime<Utc> {
        chrono::NaiveDate::from_ymd_opt(year, month, day)
            .unwrap()
            .and_hms_opt(0, 0, 0)
            .unwrap()
            .and_utc()
    }

    #[test]
    fn consolidated_types_recognized() {
        assert!(is_consolidated("trait"));
        assert!(is_consolidated("value"));
        assert!(is_consolidated("pattern"));
        assert!(is_consolidated("preference"));
        assert!(is_consolidated("mental_model"));
    }

    #[test]
    fn non_consolidated_types_rejected() {
        assert!(!is_consolidated("observation"));
        assert!(!is_consolidated("decision"));
        assert!(!is_consolidated("episode"));
        assert!(!is_consolidated(""));
    }

    #[test]
    fn day_zero_equals_confidence() {
        let now = utc(2026, 5, 8);
        let score = effective_score(0.80, None, now, now);
        assert!(
            (score - 0.80).abs() < 1e-6,
            "day-0 score should equal confidence, got {score}"
        );
    }

    #[test]
    fn half_life_day_halves_confidence() {
        let record = utc(2026, 1, 1);
        let now = record + Duration::days(180);
        let score = effective_score(0.80, None, record, now);
        let expected = 0.40;
        assert!(
            (score - expected).abs() < 1e-4,
            "at half-life, score should be ~{expected}, got {score}"
        );
    }

    #[test]
    fn year_plus_still_nonzero() {
        let record = utc(2025, 1, 1);
        let now = utc(2026, 5, 8);
        let score = effective_score(0.90, None, record, now);
        assert!(score > 0.0, "score should be nonzero even after 1+ year");
        assert!(
            score < 0.20,
            "score should be small after 1+ year, got {score}"
        );
    }

    #[test]
    fn null_last_reinforced_falls_back_to_record_time() {
        let record = utc(2026, 1, 1);
        let now = utc(2026, 7, 1);
        let score_with_none = effective_score(0.70, None, record, now);
        let score_with_record = effective_score(0.70, Some(record), record, now);
        assert!(
            (score_with_none - score_with_record).abs() < 1e-6,
            "None should fall back to record_time"
        );
    }

    #[test]
    fn last_reinforced_resets_decay_anchor() {
        let record = utc(2025, 1, 1);
        let reinforced = utc(2026, 5, 1);
        let now = utc(2026, 5, 8);
        let score = effective_score(0.80, Some(reinforced), record, now);
        assert!(
            score > 0.75,
            "recent reinforcement should keep score high, got {score}"
        );
    }

    #[test]
    fn agree_bumped_confidence_flows_through() {
        let now = utc(2026, 5, 8);
        let base = effective_score(0.70, Some(now), now, now);
        let agreed = effective_score(0.75, Some(now), now, now);
        assert!(
            agreed > base,
            "agree-bumped confidence ({agreed}) should exceed base ({base})"
        );
        assert!((agreed - 0.75).abs() < 1e-6);
    }

    #[test]
    fn disagree_dropped_confidence_flows_through() {
        let now = utc(2026, 5, 8);
        let base = effective_score(0.70, Some(now), now, now);
        let disagreed = effective_score(0.60, Some(now), now, now);
        assert!(
            disagreed < base,
            "disagree-dropped confidence ({disagreed}) should be below base ({base})"
        );
        assert!((disagreed - 0.60).abs() < 1e-6);
    }

    #[test]
    fn zero_confidence_stays_zero() {
        let record = utc(2026, 1, 1);
        let now = utc(2026, 5, 8);
        let score = effective_score(0.0, None, record, now);
        assert!((score - 0.0).abs() < 1e-6);
    }
}
