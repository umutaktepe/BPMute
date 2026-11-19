//! Enrolled Wearable persistence, allowlist queries, and lifecycle management.

use rusqlite::{Connection, OptionalExtension, params};

use crate::logging::mask_mac;
use crate::sleep_engine::WearableFloor;
use crate::storage::schema::generate_uuid_v7_with;
use crate::storage::{EnrolledWearable, StorageError, WearableDescriptor};

use super::{LedgerStore, resolve_table_id_prefix};

impl LedgerStore {
    /// Inserts or updates a Wearable by normalized MAC address + address type and returns its UUIDv7 ID.
    pub fn upsert_wearable(
        &mut self,
        desc: &WearableDescriptor,
        now_ms: i64,
    ) -> Result<String, StorageError> {
        let mac = desc.mac.trim().to_ascii_uppercase();
        if mac.is_empty() {
            return Err(StorageError::Validation(
                "wearable MAC address must not be empty".to_string(),
            ));
        }
        let address_type = if desc.address_type.trim().is_empty() {
            "public"
        } else {
            desc.address_type.trim().to_ascii_lowercase().leak()
        };

        let new_id = generate_uuid_v7_with(&self.uuid_ctx, now_ms);
        self.write_tx(|tx| {
            let existing: Option<String> = tx
                .query_row(
                    "SELECT id FROM wearables WHERE mac = ?1 AND address_type = ?2",
                    params![mac, address_type],
                    |row| row.get(0),
                )
                .optional()?;

            if let Some(id) = existing {
                tx.execute(
                    "UPDATE wearables
                     SET address_type = ?2,
                         alias = COALESCE(?3, alias),
                         advertised_name = COALESCE(?4, advertised_name),
                         supports_rr = supports_rr OR ?5,
                         supports_contact = supports_contact OR ?6,
                         last_seen_at_ms = ?7,
                         active = 1
                     WHERE id = ?1",
                    params![
                        id,
                        address_type,
                        desc.alias,
                        desc.advertised_name,
                        i32::from(desc.supports_rr),
                        i32::from(desc.supports_contact),
                        now_ms
                    ],
                )?;
                Ok(id)
            } else {
                tx.execute(
                    "INSERT INTO wearables (
                         id, mac, address_type, alias, advertised_name, supports_rr, supports_contact,
                         hr_var_floor, typical_sqi, enrolled_at_ms, last_seen_at_ms, active
                     ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, NULL, ?9, ?9, 1)",
                    params![
                        new_id,
                        mac,
                        address_type,
                        desc.alias,
                        desc.advertised_name,
                        i32::from(desc.supports_rr),
                        i32::from(desc.supports_contact),
                        WearableFloor::DEFAULT_HR_VAR_FLOOR,
                        now_ms
                    ],
                )?;
                Ok(new_id)
            }
        })
    }

    /// Lists enrolled Wearables (active allowlist entries).
    pub fn list_enrolled_wearables(&self) -> Result<Vec<EnrolledWearable>, StorageError> {
        let mut stmt = self.conn.prepare(
            "SELECT id, mac, address_type, alias, advertised_name, supports_rr, supports_contact,
                    hr_var_floor, typical_sqi, enrolled_at_ms, last_seen_at_ms, active
             FROM wearables
             WHERE active = 1
             ORDER BY last_seen_at_ms DESC, id ASC",
        )?;
        let rows = stmt
            .query_map([], |row| {
                let mac: String = row.get(1)?;
                Ok(EnrolledWearable {
                    id: row.get(0)?,
                    mac_masked: mask_mac(&mac),
                    mac,
                    address_type: row.get(2)?,
                    alias: row.get(3)?,
                    advertised_name: row.get(4)?,
                    supports_rr: row.get::<_, i64>(5)? != 0,
                    supports_contact: row.get::<_, i64>(6)? != 0,
                    hr_var_floor: row.get(7)?,
                    typical_sqi: row.get(8)?,
                    enrolled_at_ms: row.get(9)?,
                    last_seen_at_ms: row.get(10)?,
                    active: row.get::<_, i64>(11)? != 0,
                })
            })?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// Fetches an enrolled Wearable by UUIDv7 ID or unambiguous prefix.
    pub fn get_wearable(&self, id_or_prefix: &str) -> Result<EnrolledWearable, StorageError> {
        let id = resolve_wearable_id(&self.conn, id_or_prefix)?;
        load_enrolled_wearable_in_conn(&self.conn, &id)
    }

    /// Renames a Wearable's alias by UUIDv7 ID or unambiguous prefix.
    pub fn rename_wearable(
        &mut self,
        id_or_prefix: &str,
        new_alias: Option<&str>,
    ) -> Result<EnrolledWearable, StorageError> {
        self.write_tx(|tx| {
            let id = resolve_wearable_id(tx, id_or_prefix)?;
            tx.execute(
                "UPDATE wearables SET alias = ?2 WHERE id = ?1",
                params![id, new_alias],
            )?;
            load_enrolled_wearable_in_conn(tx, &id)
        })
    }

    /// Unenrolls a Wearable by UUIDv7 ID or unambiguous prefix.
    ///
    /// By default (`delete_sessions = false`), only BPMute's active allowlist entry is removed
    /// (`active = 0`), preserving historical `Monitoring Sessions`.
    /// When `delete_sessions = true`, all associated sessions and telemetry are deleted.
    pub fn unenroll_wearable(
        &mut self,
        id_or_prefix: &str,
        delete_sessions: bool,
    ) -> Result<EnrolledWearable, StorageError> {
        self.write_tx(|tx| {
            let id = resolve_wearable_id(tx, id_or_prefix)?;
            if delete_sessions {
                tx.execute(
                    "DELETE FROM session_feedback WHERE session_id IN (SELECT id FROM sessions WHERE wearable_id = ?1)",
                    params![id],
                )?;
                tx.execute(
                    "DELETE FROM session_epochs WHERE session_id IN (SELECT id FROM sessions WHERE wearable_id = ?1)",
                    params![id],
                )?;
                tx.execute(
                    "DELETE FROM raw_samples WHERE session_id IN (SELECT id FROM sessions WHERE wearable_id = ?1)",
                    params![id],
                )?;
                tx.execute(
                    "DELETE FROM sessions WHERE wearable_id = ?1",
                    params![id],
                )?;
            }
            tx.execute(
                "UPDATE wearables SET active = 0 WHERE id = ?1",
                params![id],
            )?;
            load_enrolled_wearable_in_conn(tx, &id)
        })
    }
}

pub(crate) fn resolve_wearable_id(
    conn: &Connection,
    id_or_prefix: &str,
) -> Result<String, StorageError> {
    resolve_table_id_prefix(conn, "wearables", "wearable", id_or_prefix)
}

pub(crate) fn load_enrolled_wearable_in_conn(
    conn: &Connection,
    wearable_id: &str,
) -> Result<EnrolledWearable, StorageError> {
    conn.query_row(
        "SELECT id, mac, address_type, alias, advertised_name, supports_rr, supports_contact,
                hr_var_floor, typical_sqi, enrolled_at_ms, last_seen_at_ms, active
         FROM wearables
         WHERE id = ?1",
        params![wearable_id],
        |row| {
            let mac: String = row.get(1)?;
            Ok(EnrolledWearable {
                id: row.get(0)?,
                mac_masked: mask_mac(&mac),
                mac,
                address_type: row.get(2)?,
                alias: row.get(3)?,
                advertised_name: row.get(4)?,
                supports_rr: row.get::<_, i64>(5)? != 0,
                supports_contact: row.get::<_, i64>(6)? != 0,
                hr_var_floor: row.get(7)?,
                typical_sqi: row.get(8)?,
                enrolled_at_ms: row.get(9)?,
                last_seen_at_ms: row.get(10)?,
                active: row.get::<_, i64>(11)? != 0,
            })
        },
    )
    .optional()?
    .ok_or_else(|| StorageError::NotFound(format!("wearable '{wearable_id}' not found")))
}
