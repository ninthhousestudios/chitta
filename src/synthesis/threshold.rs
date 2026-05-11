use std::collections::{HashMap, HashSet};

use chrono::{DateTime, Utc};
use uuid::Uuid;

use super::Cluster;

pub fn check_threshold(
    cluster: &Cluster,
    source_times: &HashMap<Uuid, DateTime<Utc>>,
    now: DateTime<Utc>,
    config: &super::ThresholdConfig,
) -> bool {
    if cluster.source_ids.len() < config.min_cluster_size {
        return false;
    }

    let times: Vec<&DateTime<Utc>> = cluster
        .source_ids
        .iter()
        .filter_map(|id| source_times.get(id))
        .collect();

    let distinct_days: HashSet<chrono::NaiveDate> = times.iter().map(|t| t.date_naive()).collect();
    if distinct_days.len() < config.min_distinct_days {
        return false;
    }

    let cutoff = now - chrono::Duration::days(config.max_source_age_days);
    times.iter().any(|&&t| t >= cutoff)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::synthesis::ThresholdConfig;

    fn make_source_times(ids: &[Uuid], times: &[DateTime<Utc>]) -> HashMap<Uuid, DateTime<Utc>> {
        ids.iter().copied().zip(times.iter().copied()).collect()
    }

    fn utc(year: i32, month: u32, day: u32) -> DateTime<Utc> {
        chrono::NaiveDate::from_ymd_opt(year, month, day)
            .unwrap()
            .and_hms_opt(0, 0, 0)
            .unwrap()
            .and_utc()
    }

    fn make_cluster(source_ids: Vec<Uuid>) -> Cluster {
        Cluster {
            representative_claim: "test claim".into(),
            memory_type: "trait".into(),
            source_ids,
        }
    }

    #[test]
    fn threshold_below_size() {
        let ids: Vec<Uuid> = (0..4).map(|_| Uuid::now_v7()).collect();
        let now = utc(2026, 5, 11);
        let times: Vec<DateTime<Utc>> = vec![
            utc(2026, 5, 1),
            utc(2026, 5, 2),
            utc(2026, 5, 3),
            utc(2026, 5, 4),
        ];
        let cluster = make_cluster(ids.clone());
        let source_times = make_source_times(&ids, &times);

        assert!(
            !check_threshold(&cluster, &source_times, now, &ThresholdConfig::default()),
            "4 sources should fail size>=5"
        );
    }

    #[test]
    fn threshold_exactly_at_size() {
        let ids: Vec<Uuid> = (0..5).map(|_| Uuid::now_v7()).collect();
        let now = utc(2026, 5, 11);
        let times: Vec<DateTime<Utc>> = vec![
            utc(2026, 5, 1),
            utc(2026, 5, 2),
            utc(2026, 5, 3),
            utc(2026, 5, 4),
            utc(2026, 5, 5),
        ];
        let cluster = make_cluster(ids.clone());
        let source_times = make_source_times(&ids, &times);

        assert!(
            check_threshold(&cluster, &source_times, now, &ThresholdConfig::default()),
            "exactly 5 sources across 5 days with recent data should pass"
        );
    }

    #[test]
    fn threshold_fails_distinct_days() {
        let ids: Vec<Uuid> = (0..5).map(|_| Uuid::now_v7()).collect();
        let now = utc(2026, 5, 11);
        let same_day = utc(2026, 5, 10);
        let times = vec![same_day; 5];
        let cluster = make_cluster(ids.clone());
        let source_times = make_source_times(&ids, &times);

        assert!(
            !check_threshold(&cluster, &source_times, now, &ThresholdConfig::default()),
            "all sources on same day should fail distinct_days>=2"
        );
    }

    #[test]
    fn threshold_fails_recency() {
        let ids: Vec<Uuid> = (0..5).map(|_| Uuid::now_v7()).collect();
        let now = utc(2026, 5, 11);
        let times: Vec<DateTime<Utc>> = vec![
            utc(2025, 1, 1),
            utc(2025, 1, 2),
            utc(2025, 1, 3),
            utc(2025, 1, 4),
            utc(2025, 1, 5),
        ];
        let cluster = make_cluster(ids.clone());
        let source_times = make_source_times(&ids, &times);

        assert!(
            !check_threshold(&cluster, &source_times, now, &ThresholdConfig::default()),
            "all sources older than 90 days should fail recency check"
        );
    }

    #[test]
    fn threshold_happy_path() {
        let ids: Vec<Uuid> = (0..7).map(|_| Uuid::now_v7()).collect();
        let now = utc(2026, 5, 11);
        let times: Vec<DateTime<Utc>> = vec![
            utc(2025, 12, 1),
            utc(2025, 12, 15),
            utc(2026, 1, 10),
            utc(2026, 3, 5),
            utc(2026, 4, 20),
            utc(2026, 5, 1),
            utc(2026, 5, 10),
        ];
        let cluster = make_cluster(ids.clone());
        let source_times = make_source_times(&ids, &times);

        assert!(
            check_threshold(&cluster, &source_times, now, &ThresholdConfig::default()),
            "7 sources across many days with recent entries should pass all checks"
        );
    }
}
