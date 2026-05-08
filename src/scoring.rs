use chrono::{DateTime, Utc};

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
    fn day_zero_equals_confidence() {
        let now = utc(2026, 5, 8);
        let score = effective_score(0.80, None, now, now);
        assert!((score - 0.80).abs() < 1e-6, "day-0 score should equal confidence, got {score}");
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
        assert!(score < 0.20, "score should be small after 1+ year, got {score}");
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
        // Only 7 days of decay from reinforcement, not 490+ from record_time
        assert!(score > 0.75, "recent reinforcement should keep score high, got {score}");
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
