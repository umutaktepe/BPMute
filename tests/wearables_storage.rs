use bpmute::config::Sensitivity;
use bpmute::storage::{
    LedgerStore, OpenSessionRequest, WearableDescriptor, db_path_in_data_dir, lock_path_in_data_dir,
};
use tempfile::tempdir;

#[test]
fn wearable_storage_allowlist_rename_and_unenroll_lifecycle() {
    let dir = tempdir().unwrap();
    let db_path = db_path_in_data_dir(dir.path());
    let lock_path = lock_path_in_data_dir(dir.path());

    let mut store = LedgerStore::open_read_write(&db_path, &lock_path).unwrap();

    // 1. Upsert wearable with lowercase MAC and address_type "random"
    let desc = WearableDescriptor {
        mac: "aa:bb:cc:dd:ee:ff".to_string(),
        address_type: "random".to_string(),
        alias: Some("My Watch".to_string()),
        advertised_name: Some("Huawei GT5".to_string()),
        supports_rr: false,
        supports_contact: true,
    };
    let w_id = store.upsert_wearable(&desc, 1_000).unwrap();

    // Verify normalization to uppercase
    let list = store.list_enrolled_wearables().unwrap();
    assert_eq!(list.len(), 1);
    assert_eq!(list[0].id, w_id);
    assert_eq!(list[0].mac, "AA:BB:CC:DD:EE:FF");
    assert_eq!(list[0].address_type, "random");
    assert_eq!(list[0].alias.as_deref(), Some("My Watch"));
    assert!(list[0].active);

    // 2. Rename wearable via prefix
    let prefix = &w_id[..8];
    let updated = store.rename_wearable(prefix, Some("Sleep Watch")).unwrap();
    assert_eq!(updated.alias.as_deref(), Some("Sleep Watch"));

    // 3. Open a session for this wearable
    let session_id = store
        .open_session(&OpenSessionRequest {
            wearable_id: w_id.clone(),
            start_ts_ms: 2_000,
            detection_sensitivity: Sensitivity::Balanced,
        })
        .unwrap();

    // 4. Unenroll without --delete-sessions (default: removes from allowlist, keeps sessions)
    let unenrolled = store.unenroll_wearable(prefix, false).unwrap();
    assert!(!unenrolled.active);

    let active_list = store.list_enrolled_wearables().unwrap();
    assert!(
        active_list.is_empty(),
        "unenrolled wearable must not appear in active allowlist"
    );

    // Session must still exist
    let session = store.get_session(&session_id).unwrap();
    assert_eq!(session.session.id, session_id);

    // 5. Re-enroll the wearable
    let re_id = store.upsert_wearable(&desc, 3_000).unwrap();
    assert_eq!(re_id, w_id);
    assert_eq!(store.list_enrolled_wearables().unwrap().len(), 1);

    // 6. Unenroll with delete_sessions = true
    let unenrolled_purge = store.unenroll_wearable(prefix, true).unwrap();
    assert!(!unenrolled_purge.active);
    assert!(
        store.get_session(&session_id).is_err(),
        "session must be purged with delete_sessions = true"
    );
}
