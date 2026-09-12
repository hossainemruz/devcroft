use super::*;
#[test]
fn refresh_coalesces_requests_and_discards_superseded_results() {
    let mut refresh = Refresh::default();
    let first = refresh.request().unwrap();
    assert!(refresh.request().is_none());
    assert!(refresh.request().is_none());
    assert!(refresh.pending);
    assert!(!refresh.finish(first));
    let latest = refresh.request().unwrap();
    assert!(!refresh.pending);
    assert!(refresh.finish(latest));
    assert!(!refresh.busy);
}

#[test]
fn kind_labels_cover_every_filter_option() {
    assert_eq!(Kind::ALL.len(), 3);
    assert_eq!(Kind::Rfc.label(), "RFC");
    assert_eq!(Kind::Plan.label(), "Plan");
    assert_eq!(Kind::Note.label(), "Note");
}

#[test]
fn updated_label_buckets_like_relative_durations() {
    let now_secs = 1_700_000_000;
    let ms = |secs_ago: i64| ((now_secs - secs_ago) as u64).saturating_mul(1_000);
    assert_eq!(updated_label(ms(0), now_secs), "Updated just now");
    assert_eq!(updated_label(ms(30), now_secs), "Updated just now");
    assert_eq!(updated_label(ms(300), now_secs), "Updated 5m ago");
    assert_eq!(updated_label(ms(7_200), now_secs), "Updated 2h ago");
    assert_eq!(updated_label(ms(259_200), now_secs), "Updated 3d ago");
    // Future timestamps never render a negative duration.
    assert_eq!(
        updated_label(((now_secs + 600) as u64).saturating_mul(1_000), now_secs),
        "Updated just now"
    );
}
