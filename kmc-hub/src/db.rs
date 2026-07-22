use anyhow::{anyhow, Context, Result};
use argon2::password_hash::{rand_core::OsRng, PasswordHash, PasswordHasher, PasswordVerifier, SaltString};
use argon2::Argon2;
use chrono::Utc;
use rusqlite::{Connection, OptionalExtension};
use uuid::Uuid;

/// laptops 행 (프로비저닝 결과)
pub struct Laptop {
    pub agent_id: Uuid,
    pub name: String,
    pub provision_token: String,
}

pub fn open(path: &str) -> Result<Connection> {
    let conn = Connection::open(path).with_context(|| format!("open db {path}"))?;
    init_schema(&conn)?;
    Ok(conn)
}

fn init_schema(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        r#"
        CREATE TABLE IF NOT EXISTS laptops(
            agent_id TEXT PRIMARY KEY,
            mac TEXT UNIQUE NOT NULL,
            name TEXT NOT NULL,
            provision_token TEXT NOT NULL,
            created_at TEXT NOT NULL
        );
        CREATE TABLE IF NOT EXISTS admins(
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            username TEXT UNIQUE NOT NULL,
            password_hash TEXT NOT NULL,
            created_at TEXT NOT NULL
        );
        "#,
    )?;
    Ok(())
}

/// 동일 MAC이면 기존 행 반환(멱등). 신규면 student-<NN> 발급.
pub fn get_or_create_laptop(conn: &Connection, mac: &str) -> Result<Laptop> {
    if let Some(l) = conn
        .query_row(
            "SELECT agent_id, name, provision_token FROM laptops WHERE mac = ?1",
            [mac],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                ))
            },
        )
        .optional()?
    {
        let agent_id = Uuid::parse_str(&l.0).map_err(|e| anyhow!("bad agent_id in db: {e}"))?;
        return Ok(Laptop { agent_id, name: l.1, provision_token: l.2 });
    }

    let count: i64 = conn.query_row("SELECT COUNT(*) FROM laptops", [], |r| r.get(0))?;
    let name = format!("student-{:02}", count + 1);
    let agent_id = Uuid::new_v4();
    let provision_token = crate::util::random_hex(32);
    let created_at = Utc::now().to_rfc3339();
    conn.execute(
        "INSERT INTO laptops(agent_id, mac, name, provision_token, created_at) VALUES(?1,?2,?3,?4,?5)",
        rusqlite::params![agent_id.to_string(), mac, name, provision_token, created_at],
    )?;
    Ok(Laptop { agent_id, name, provision_token })
}

/// agent_id + provision_token 검증 (WS Hello). 일치하는 laptops 행이 있으면 name 반환.
pub fn verify_agent(conn: &Connection, agent_id: Uuid, provision_token: &str) -> Result<Option<String>> {
    let name = conn
        .query_row(
            "SELECT name FROM laptops WHERE agent_id = ?1 AND provision_token = ?2",
            rusqlite::params![agent_id.to_string(), provision_token],
            |r| r.get::<_, String>(0),
        )
        .optional()?;
    Ok(name)
}

/// 등록된 모든 laptops (agent_id, name)
pub fn all_laptops(conn: &Connection) -> Result<Vec<(Uuid, String)>> {
    let mut stmt = conn.prepare("SELECT agent_id, name FROM laptops ORDER BY name")?;
    let rows = stmt.query_map([], |row| {
        Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
    })?;
    let mut out = Vec::new();
    for r in rows {
        let (id, name) = r?;
        let id = Uuid::parse_str(&id).map_err(|e| anyhow!("bad agent_id in db: {e}"))?;
        out.push((id, name));
    }
    Ok(out)
}

pub fn laptop_name(conn: &Connection, agent_id: Uuid) -> Result<Option<String>> {
    let name = conn
        .query_row(
            "SELECT name FROM laptops WHERE agent_id = ?1",
            [agent_id.to_string()],
            |r| r.get::<_, String>(0),
        )
        .optional()?;
    Ok(name)
}

/// argon2 검증. 성공 시 admin id.
pub fn verify_admin(conn: &Connection, username: &str, password: &str) -> Result<Option<i64>> {
    let row = conn
        .query_row(
            "SELECT id, password_hash FROM admins WHERE username = ?1",
            [username],
            |r| Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?)),
        )
        .optional()?;
    let Some((id, hash)) = row else { return Ok(None) };
    let parsed = PasswordHash::new(&hash).map_err(|e| anyhow!("bad hash in db: {e}"))?;
    match Argon2::default().verify_password(password.as_bytes(), &parsed) {
        Ok(()) => Ok(Some(id)),
        Err(_) => Ok(None),
    }
}

pub fn add_admin(conn: &Connection, username: &str, password: &str) -> Result<()> {
    let salt = SaltString::generate(&mut OsRng);
    let hash = Argon2::default()
        .hash_password(password.as_bytes(), &salt)
        .map_err(|e| anyhow!("hash password: {e}"))?
        .to_string();
    let created_at = Utc::now().to_rfc3339();
    conn.execute(
        "INSERT INTO admins(username, password_hash, created_at) VALUES(?1,?2,?3)",
        rusqlite::params![username, hash, created_at],
    )
    .with_context(|| format!("insert admin {username}"))?;
    Ok(())
}
