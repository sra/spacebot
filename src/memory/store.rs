//! Memory graph storage (SQLite).

use crate::error::{MemoryError, Result};
use crate::memory::types::{Association, Memory, MemoryType, RelationType};
use std::sync::Arc;
use sqlx::{Row, SqlitePool};
use anyhow::Context as _;

/// Memory store for CRUD and graph operations.
pub struct MemoryStore {
    pool: SqlitePool,
}

impl std::fmt::Debug for MemoryStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MemoryStore")
            .field("pool", &"<SqlitePool>")
            .finish()
    }
}

impl MemoryStore {
    /// Create a new memory store with the given SQLite pool.
    pub fn new(pool: SqlitePool) -> Arc<Self> {
        Arc::new(Self { pool })
    }
    
    /// Get a reference to the SQLite pool.
    pub(crate) fn pool(&self) -> &SqlitePool {
        &self.pool
    }
    
    /// Save a new memory to the store.
    pub async fn save(&self, memory: &Memory) -> Result<()> {
        sqlx::query(
            r#"
            INSERT INTO memories (id, content, memory_type, importance, created_at, updated_at, 
                                 last_accessed_at, access_count, source, channel_id, forgotten)
            VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
            "#
        )
        .bind(&memory.id)
        .bind(&memory.content)
        .bind(memory.memory_type.to_string())
        .bind(memory.importance)
        .bind(memory.created_at)
        .bind(memory.updated_at)
        .bind(memory.last_accessed_at)
        .bind(memory.access_count)
        .bind(&memory.source)
        .bind(memory.channel_id.as_ref().map(|id| id.as_ref()))
        .bind(memory.forgotten)
        .execute(&self.pool)
        .await
        .with_context(|| format!("failed to save memory {}", memory.id))?;
        
        Ok(())
    }
    
    /// Load a memory by ID.
    pub async fn load(&self, id: &str) -> Result<Option<Memory>> {
        let row = sqlx::query(
            r#"
            SELECT id, content, memory_type, importance, created_at, updated_at,
                   last_accessed_at, access_count, source, channel_id, forgotten
            FROM memories
            WHERE id = ?
            "#
        )
        .bind(id)
        .fetch_optional(&self.pool)
        .await
        .with_context(|| format!("failed to load memory {}", id))?;
        
        Ok(row.map(|row| row_to_memory(&row)))
    }
    
    /// Update an existing memory.
    pub async fn update(&self, memory: &Memory) -> Result<()> {
        sqlx::query(
            r#"
            UPDATE memories 
            SET content = ?, memory_type = ?, importance = ?, updated_at = ?, 
                last_accessed_at = ?, access_count = ?, source = ?, channel_id = ?,
                forgotten = ?
            WHERE id = ?
            "#
        )
        .bind(&memory.content)
        .bind(memory.memory_type.to_string())
        .bind(memory.importance)
        .bind(memory.updated_at)
        .bind(memory.last_accessed_at)
        .bind(memory.access_count)
        .bind(&memory.source)
        .bind(memory.channel_id.as_ref().map(|id| id.as_ref()))
        .bind(memory.forgotten)
        .bind(&memory.id)
        .execute(&self.pool)
        .await
        .with_context(|| format!("failed to update memory {}", memory.id))?;
        
        Ok(())
    }
    
    /// Delete a memory by ID.
    pub async fn delete(&self, id: &str) -> Result<()> {
        sqlx::query("DELETE FROM memories WHERE id = ?")
            .bind(id)
            .execute(&self.pool)
            .await
            .with_context(|| format!("failed to delete memory {}", id))?;
        
        Ok(())
    }
    
    /// Record access to a memory, updating last_accessed_at and access_count.
    pub async fn record_access(&self, id: &str) -> Result<()> {
        let now = chrono::Utc::now();
        
        sqlx::query(
            r#"
            UPDATE memories 
            SET last_accessed_at = ?, access_count = access_count + 1
            WHERE id = ?
            "#
        )
        .bind(now)
        .bind(id)
        .execute(&self.pool)
        .await
        .with_context(|| format!("failed to record access for memory {}", id))?;
        
        Ok(())
    }
    
    /// Mark a memory as forgotten. The memory stays in the database but is
    /// excluded from search results and recall.
    pub async fn forget(&self, id: &str) -> Result<bool> {
        let result = sqlx::query(
            "UPDATE memories SET forgotten = 1, updated_at = ? WHERE id = ? AND forgotten = 0"
        )
        .bind(chrono::Utc::now())
        .bind(id)
        .execute(&self.pool)
        .await
        .with_context(|| format!("failed to forget memory {}", id))?;

        Ok(result.rows_affected() > 0)
    }

    /// Create an association between two memories.
    pub async fn create_association(&self, association: &Association) -> Result<()> {
        sqlx::query(
            r#"
            INSERT INTO associations (id, source_id, target_id, relation_type, weight, created_at)
            VALUES (?, ?, ?, ?, ?, ?)
            ON CONFLICT(source_id, target_id, relation_type) DO UPDATE SET
                weight = excluded.weight
            "#
        )
        .bind(&association.id)
        .bind(&association.source_id)
        .bind(&association.target_id)
        .bind(association.relation_type.to_string())
        .bind(association.weight)
        .bind(association.created_at)
        .execute(&self.pool)
        .await
        .with_context(|| {
            format!(
                "failed to create association from {} to {}",
                association.source_id, association.target_id
            )
        })?;
        
        Ok(())
    }
    
    /// Get all associations for a memory (both incoming and outgoing).
    pub async fn get_associations(&self, memory_id: &str) -> Result<Vec<Association>> {
        let rows = sqlx::query(
            r#"
            SELECT id, source_id, target_id, relation_type, weight, created_at
            FROM associations
            WHERE source_id = ? OR target_id = ?
            "#
        )
        .bind(memory_id)
        .bind(memory_id)
        .fetch_all(&self.pool)
        .await
        .with_context(|| format!("failed to get associations for memory {}", memory_id))?;
        
        let associations = rows
            .into_iter()
            .map(|row| row_to_association(&row))
            .collect();
        
        Ok(associations)
    }
    
    /// Get memories by type.
    pub async fn get_by_type(&self, memory_type: MemoryType, limit: i64) -> Result<Vec<Memory>> {
        let type_str = memory_type.to_string();
        
        let rows = sqlx::query(
            r#"
            SELECT id, content, memory_type, importance, created_at, updated_at,
                   last_accessed_at, access_count, source, channel_id, forgotten
            FROM memories
            WHERE memory_type = ? AND forgotten = 0
            ORDER BY importance DESC, updated_at DESC
            LIMIT ?
            "#
        )
        .bind(&type_str)
        .bind(limit)
        .fetch_all(&self.pool)
        .await
        .with_context(|| format!("failed to get memories by type {:?}", memory_type))?;
        
        Ok(rows.into_iter().map(|row| row_to_memory(&row)).collect())
    }
    
    /// Get high-importance memories for injection into context.
    pub async fn get_high_importance(&self, threshold: f32, limit: i64) -> Result<Vec<Memory>> {
        let rows = sqlx::query(
            r#"
            SELECT id, content, memory_type, importance, created_at, updated_at,
                   last_accessed_at, access_count, source, channel_id, forgotten
            FROM memories
            WHERE importance >= ? AND forgotten = 0
            ORDER BY importance DESC, updated_at DESC
            LIMIT ?
            "#
        )
        .bind(threshold)
        .bind(limit)
        .fetch_all(&self.pool)
        .await
        .with_context(|| "failed to get high importance memories")?;
        
        Ok(rows.into_iter().map(|row| row_to_memory(&row)).collect())
    }
    
}

/// Helper: Convert a database row to a Memory.
fn row_to_memory(row: &sqlx::sqlite::SqliteRow) -> Memory {
    let mem_type_str: String = row.try_get("memory_type").unwrap_or_default();
    let memory_type = parse_memory_type(&mem_type_str);
    
    let channel_id: Option<String> = row.try_get("channel_id").ok();
    
    Memory {
        id: row.try_get("id").unwrap_or_default(),
        content: row.try_get("content").unwrap_or_default(),
        memory_type,
        importance: row.try_get("importance").unwrap_or(0.5),
        created_at: row.try_get("created_at").unwrap_or_else(|_| chrono::Utc::now()),
        updated_at: row.try_get("updated_at").unwrap_or_else(|_| chrono::Utc::now()),
        last_accessed_at: row.try_get("last_accessed_at").unwrap_or_else(|_| chrono::Utc::now()),
        access_count: row.try_get("access_count").unwrap_or(0),
        source: row.try_get("source").ok(),
        channel_id: channel_id.map(|id| Arc::from(id) as crate::ChannelId),
        forgotten: row.try_get::<bool, _>("forgotten").unwrap_or(false),
    }
}

/// Helper: Parse memory type from string.
fn parse_memory_type(s: &str) -> MemoryType {
    match s {
        "fact" => MemoryType::Fact,
        "preference" => MemoryType::Preference,
        "decision" => MemoryType::Decision,
        "identity" => MemoryType::Identity,
        "event" => MemoryType::Event,
        "observation" => MemoryType::Observation,
        "goal" => MemoryType::Goal,
        _ => MemoryType::Fact,
    }
}

/// Helper: Convert a database row to an Association.
fn row_to_association(row: &sqlx::sqlite::SqliteRow) -> Association {
    let relation_type_str: String = row.try_get("relation_type").unwrap_or_default();
    let relation_type = parse_relation_type(&relation_type_str);
    
    Association {
        id: row.try_get("id").unwrap_or_default(),
        source_id: row.try_get("source_id").unwrap_or_default(),
        target_id: row.try_get("target_id").unwrap_or_default(),
        relation_type,
        weight: row.try_get("weight").unwrap_or(0.5),
        created_at: row.try_get("created_at").unwrap_or_else(|_| chrono::Utc::now()),
    }
}

/// Helper: Parse relation type from string.
fn parse_relation_type(s: &str) -> RelationType {
    match s {
        "related_to" => RelationType::RelatedTo,
        "updates" => RelationType::Updates,
        "contradicts" => RelationType::Contradicts,
        "caused_by" => RelationType::CausedBy,
        "result_of" => RelationType::ResultOf,
        "part_of" => RelationType::PartOf,
        _ => RelationType::RelatedTo,
    }
}
