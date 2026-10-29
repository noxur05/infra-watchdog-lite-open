use sqlx::SqlitePool;

#[derive(Debug, Clone)]
pub struct SelectelAccount {
    pub id: i64,
    pub name: String,
    pub username: String,
    pub password: String,
    pub account_id: String,
    pub project_id: String,
    pub region: String,
    pub is_active: bool,
    pub created_at: String,
}

pub async fn list_active_accounts(pool: &SqlitePool) -> Result<Vec<SelectelAccount>, sqlx::Error> {
    let rows: Vec<(i64, String, String, String, String, String, String, bool, String)> = sqlx::query_as(
        "SELECT id, name, username, password, account_id, project_id, region, is_active, created_at
         FROM selectel_accounts WHERE is_active = 1 ORDER BY id"
    )
    .fetch_all(pool)
    .await?;

    Ok(rows.into_iter().map(|r| SelectelAccount {
        id: r.0, name: r.1, username: r.2, password: r.3,
        account_id: r.4, project_id: r.5, region: r.6,
        is_active: r.7, created_at: r.8,
    }).collect())
}

pub async fn list_all_accounts(pool: &SqlitePool) -> Result<Vec<SelectelAccount>, sqlx::Error> {
    let rows: Vec<(i64, String, String, String, String, String, String, bool, String)> = sqlx::query_as(
        "SELECT id, name, username, password, account_id, project_id, region, is_active, created_at
         FROM selectel_accounts ORDER BY id"
    )
    .fetch_all(pool)
    .await?;

    Ok(rows.into_iter().map(|r| SelectelAccount {
        id: r.0, name: r.1, username: r.2, password: r.3,
        account_id: r.4, project_id: r.5, region: r.6,
        is_active: r.7, created_at: r.8,
    }).collect())
}

pub async fn get_account(pool: &SqlitePool, id: i64) -> Result<Option<SelectelAccount>, sqlx::Error> {
    let row: Option<(i64, String, String, String, String, String, String, bool, String)> = sqlx::query_as(
        "SELECT id, name, username, password, account_id, project_id, region, is_active, created_at
         FROM selectel_accounts WHERE id = ?"
    )
    .bind(id)
    .fetch_optional(pool)
    .await?;

    Ok(row.map(|r| SelectelAccount {
        id: r.0, name: r.1, username: r.2, password: r.3,
        account_id: r.4, project_id: r.5, region: r.6,
        is_active: r.7, created_at: r.8,
    }))
}

pub async fn add_account(
    pool: &SqlitePool,
    name: &str,
    username: &str,
    password: &str,
    account_id: &str,
    project_id: &str,
    region: &str,
) -> Result<i64, sqlx::Error> {
    let row: (i64,) = sqlx::query_as(
        "INSERT INTO selectel_accounts (name, username, password, account_id, project_id, region)
         VALUES (?, ?, ?, ?, ?, ?) RETURNING id"
    )
    .bind(name)
    .bind(username)
    .bind(password)
    .bind(account_id)
    .bind(project_id)
    .bind(region)
    .fetch_one(pool)
    .await?;
    Ok(row.0)
}

pub async fn deactivate_account(pool: &SqlitePool, id: i64) -> Result<bool, sqlx::Error> {
    crate::db::set_account_active(pool, "selectel_accounts", id, false).await
}

/// Hard-delete a deactivated Selectel account row + its failover regions.
/// The migration declares `ON DELETE CASCADE`, but PRAGMA foreign_keys may
/// be off at runtime, so wipe the children explicitly too.
pub async fn delete_account(pool: &SqlitePool, id: i64) -> Result<bool, sqlx::Error> {
    sqlx::query("DELETE FROM selectel_account_regions WHERE account_id = ?")
        .bind(id).execute(pool).await?;
    crate::db::delete_deactivated_account(pool, "selectel_accounts", id).await
}

pub async fn activate_account(pool: &SqlitePool, id: i64) -> Result<bool, sqlx::Error> {
    crate::db::set_account_active(pool, "selectel_accounts", id, true).await
}

pub async fn update_region(pool: &SqlitePool, id: i64, region: &str) -> Result<bool, sqlx::Error> {
    let result = sqlx::query("UPDATE selectel_accounts SET region = ? WHERE id = ?")
        .bind(region)
        .bind(id)
        .execute(pool)
        .await?;
    Ok(result.rows_affected() > 0)
}

// ── Multi-region failover (selectel_account_regions) ──

/// List regions configured for an account, ordered by priority (1 first).
pub async fn list_account_regions(pool: &SqlitePool, account_id: i64) -> Result<Vec<(String, i64)>, sqlx::Error> {
    let rows: Vec<(String, i64)> = sqlx::query_as(
        "SELECT region_id, priority FROM selectel_account_regions
         WHERE account_id = ? ORDER BY priority"
    )
    .bind(account_id)
    .fetch_all(pool)
    .await?;
    Ok(rows)
}

/// Insert (account_id, region_id, priority). Idempotent — does nothing if row exists.
pub async fn add_account_region(pool: &SqlitePool, account_id: i64, region_id: &str, priority: i64) -> Result<(), sqlx::Error> {
    sqlx::query(
        "INSERT OR IGNORE INTO selectel_account_regions (account_id, region_id, priority)
         VALUES (?, ?, ?)"
    )
    .bind(account_id)
    .bind(region_id)
    .bind(priority)
    .execute(pool)
    .await?;
    Ok(())
}

/// Remove a region from an account's priority list.
pub async fn remove_account_region(pool: &SqlitePool, account_id: i64, region_id: &str) -> Result<bool, sqlx::Error> {
    let result = sqlx::query(
        "DELETE FROM selectel_account_regions WHERE account_id = ? AND region_id = ?"
    )
    .bind(account_id)
    .bind(region_id)
    .execute(pool)
    .await?;
    Ok(result.rows_affected() > 0)
}


/// Highest existing priority + 1, or 1 if no regions yet. Used when adding a new region.
pub async fn next_priority(pool: &SqlitePool, account_id: i64) -> Result<i64, sqlx::Error> {
    let row: (i64,) = sqlx::query_as(
        "SELECT COALESCE(MAX(priority), 0) FROM selectel_account_regions WHERE account_id = ?"
    )
    .bind(account_id)
    .fetch_one(pool)
    .await?;
    Ok(row.0 + 1)
}
