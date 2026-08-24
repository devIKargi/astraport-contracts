//! Migration logic and backward-compatibility checks for the versioning contract.
//!
//! Provides helpers for validating that a proposed upgrade is compatible,
//! executing ordered migration steps, and recording migration outcomes.

use soroban_sdk::{contracttype, symbol_short, Env};

use crate::records::{MigrationRecord, VersionMetadata, VersionStatus, VersionStorageKey};

/// Errors specific to migration operations.
#[contracttype]
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
#[repr(u32)]
pub enum MigrationError {
    /// The target version does not exist or has no metadata.
    TargetVersionNotFound = 1,
    /// Trying to migrate to a version that is not in `Proposed` or `Superseded` status.
    InvalidTargetStatus = 2,
    /// Trying to migrate from a version that is not the current active version.
    NotCurrentVersion = 3,
    /// The target version is frozen and cannot be activated.
    VersionFrozen = 4,
    /// A migration between these two versions has already been recorded.
    MigrationAlreadyRecorded = 5,
    /// Backward-compatibility check failed: the target version is not compatible.
    IncompatibleVersion = 6,
    /// Migration steps list is empty (at least one step is required).
    NoMigrationSteps = 7,
}

// ---------------------------------------------------------------------------
// Compatibility check
// ---------------------------------------------------------------------------

/// Checks whether upgrading from `from_version` to `to_version` is valid.
///
/// Returns `Ok(())` if the upgrade is allowed, or a descriptive
/// `MigrationError` otherwise.
pub fn check_compatibility(
    env: &Env,
    from_version: u32,
    to_version: u32,
) -> Result<(), MigrationError> {
    // A version cannot upgrade to itself.
    if from_version == to_version {
        return Err(MigrationError::IncompatibleVersion);
    }

    // Bootstrap case: from_version == 0 means no prior version exists.
    // We only validate the target version.
    let to_meta: Option<VersionMetadata> = env
        .storage()
        .persistent()
        .get(&VersionStorageKey::VersionMetadata(to_version));
    let to_meta = to_meta.ok_or(MigrationError::TargetVersionNotFound)?;

    if from_version == 0 {
        // Bootstrap: validate target status and steps, but skip from-version checks.
        if to_meta.status != VersionStatus::Proposed {
            return Err(MigrationError::InvalidTargetStatus);
        }
        if to_meta.migration_steps.is_empty() {
            return Err(MigrationError::NoMigrationSteps);
        }
        return Ok(());
    }

    // Validate the "from" version has metadata.
    let from_meta: Option<VersionMetadata> = env
        .storage()
        .persistent()
        .get(&VersionStorageKey::VersionMetadata(from_version));
    if from_meta.is_none() {
        return Err(MigrationError::TargetVersionNotFound);
    }

    // The target version must be `Proposed` (not yet active) or `Superseded`
    // (reactivating a previous version).
    if to_meta.status != VersionStatus::Proposed && to_meta.status != VersionStatus::Superseded {
        return Err(MigrationError::InvalidTargetStatus);
    }

    // A frozen target version cannot be activated.
    if to_meta.status == VersionStatus::Frozen {
        return Err(MigrationError::VersionFrozen);
    }

    // The "from" version must be the currently active version.
    let current: u32 = env
        .storage()
        .persistent()
        .get(&VersionStorageKey::CurrentVersion)
        .unwrap_or(0);
    if from_version != current {
        return Err(MigrationError::NotCurrentVersion);
    }

    // Target version must have at least one migration step defined.
    if to_meta.migration_steps.is_empty() {
        return Err(MigrationError::NoMigrationSteps);
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// Migration execution
// ---------------------------------------------------------------------------

/// Executes the migration from `from_version` to `to_version`.
///
/// This function:
/// 1. Validates compatibility via [`check_compatibility`].
/// 2. Iterates through the migration steps in the target version metadata.
/// 3. Marks the old version as `Superseded` and the new version as `Active`.
/// 4. Records the migration outcome.
///
/// Returns the completed [`MigrationRecord`].
pub fn execute_migration(
    env: &Env,
    from_version: u32,
    to_version: u32,
    migrator: soroban_sdk::Address,
) -> Result<MigrationRecord, MigrationError> {
    // Validate compatibility.
    check_compatibility(env, from_version, to_version)?;

    // Load target metadata.
    let mut to_meta: VersionMetadata = env
        .storage()
        .persistent()
        .get(&VersionStorageKey::VersionMetadata(to_version))
        .ok_or(MigrationError::TargetVersionNotFound)?;

    // Execute migration steps (count them for the record).
    let steps_count = to_meta.migration_steps.len();

    // Iterate through migration steps and record each one.
    let mut i: u32 = 0;
    while i < steps_count {
        let step = to_meta.migration_steps.get(i).unwrap();
        env.events()
            .publish((symbol_short!("MIG_STEP"), from_version, to_version), step);
        i += 1;
    }

    // Mark the old version as Superseded (skip for bootstrap from version 0).
    if from_version > 0 {
        let mut from_meta: VersionMetadata = env
            .storage()
            .persistent()
            .get(&VersionStorageKey::VersionMetadata(from_version))
            .ok_or(MigrationError::TargetVersionNotFound)?;
        from_meta.status = VersionStatus::Superseded;
        env.storage().persistent().set(
            &VersionStorageKey::VersionMetadata(from_version),
            &from_meta,
        );
    }

    let now = env.ledger().timestamp();
    to_meta.status = VersionStatus::Active;
    to_meta.activated_at = now;
    env.storage()
        .persistent()
        .set(&VersionStorageKey::VersionMetadata(to_version), &to_meta);

    // Update current version pointer.
    env.storage()
        .persistent()
        .set(&VersionStorageKey::CurrentVersion, &to_version);

    // Record migration.
    let record = MigrationRecord {
        from_version,
        to_version,
        migrator,
        timestamp: now,
        success: true,
        items_migrated: steps_count as u64,
    };
    env.storage().persistent().set(
        &VersionStorageKey::MigrationRecord(from_version, to_version),
        &record,
    );

    // Emit migration-complete event.
    env.events().publish(
        (symbol_short!("MIG_DONE"), from_version, to_version),
        record.clone(),
    );

    Ok(record)
}

// ---------------------------------------------------------------------------
// Rollback
// ---------------------------------------------------------------------------

/// Rolls back from `current_version` to `previous_version`.
///
/// The previous version must have been `Superseded` (i.e. it was the version
/// active before the current one). The current version is set to `RolledBack`
/// and the previous version is restored to `Active`.
pub fn execute_rollback(
    env: &Env,
    current_version: u32,
    previous_version: u32,
    admin: soroban_sdk::Address,
) -> Result<MigrationRecord, MigrationError> {
    // Validate versions exist.
    let current_meta: VersionMetadata = env
        .storage()
        .persistent()
        .get(&VersionStorageKey::VersionMetadata(current_version))
        .ok_or(MigrationError::TargetVersionNotFound)?;

    let mut prev_meta: VersionMetadata = env
        .storage()
        .persistent()
        .get(&VersionStorageKey::VersionMetadata(previous_version))
        .ok_or(MigrationError::TargetVersionNotFound)?;

    // The current version must actually be active.
    if current_meta.status != VersionStatus::Active {
        return Err(MigrationError::InvalidTargetStatus);
    }

    // The previous version must be superseded (it was replaced by the current one).
    if prev_meta.status != VersionStatus::Superseded {
        return Err(MigrationError::InvalidTargetStatus);
    }

    let now = env.ledger().timestamp();

    // Mark current version as rolled back.
    let mut current_meta = current_meta;
    current_meta.status = VersionStatus::RolledBack;
    env.storage().persistent().set(
        &VersionStorageKey::VersionMetadata(current_version),
        &current_meta,
    );

    // Restore previous version to active.
    prev_meta.status = VersionStatus::Active;
    prev_meta.activated_at = now;
    env.storage().persistent().set(
        &VersionStorageKey::VersionMetadata(previous_version),
        &prev_meta,
    );

    // Update current version pointer.
    env.storage()
        .persistent()
        .set(&VersionStorageKey::CurrentVersion, &previous_version);

    // Record rollback as a migration record (from current → previous).
    let record = MigrationRecord {
        from_version: current_version,
        to_version: previous_version,
        migrator: admin,
        timestamp: now,
        success: true,
        items_migrated: 0, // Rollback doesn't migrate data
    };
    env.storage().persistent().set(
        &VersionStorageKey::MigrationRecord(current_version, previous_version),
        &record,
    );

    // Emit rollback event.
    env.events().publish(
        (symbol_short!("ROLLBACK"), current_version, previous_version),
        record.clone(),
    );

    Ok(record)
}
