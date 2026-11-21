//! Exact fixed-scale decimal functions for SQLite connections.
//!
//! SQLite has no exact decimal storage class. SDKWork stores logical decimal
//! values as canonical text in SQLite, then uses these functions whenever SQL
//! must aggregate or order those values. This prevents SQLite from silently
//! coercing ledger values through binary floating point.

use std::ffi::c_int;
use std::io;
use std::mem::size_of;
use std::ptr;

use libsqlite3_sys as ffi;
use sqlx::sqlite::SqliteConnection;

const SCALE: u32 = 12;
const SCALE_FACTOR: i128 = 1_000_000_000_000;
const MAX_PRECISION: usize = 38;

const DECIMAL_SUM_NAME: &[u8] = b"sdkwork_decimal_sum\0";
const DECIMAL_ORDER_KEY_NAME: &[u8] = b"sdkwork_decimal_order_key\0";

const INVALID_ARGUMENT_MESSAGE: &[u8] =
    b"sdkwork decimal function received an invalid canonical decimal";
const OVERFLOW_MESSAGE: &[u8] = b"sdkwork decimal aggregation overflow";
const MEMORY_MESSAGE: &[u8] = b"sdkwork decimal aggregation could not allocate state";

#[repr(C)]
#[derive(Default)]
struct DecimalSumState {
    scaled: i128,
    failed: bool,
}

/// Register SDKWork exact decimal functions on one SQLite connection.
///
/// Registration is connection-local and idempotent. Callers that construct a
/// pool outside [`crate::sqlite::create_sqlite_pool`] must register functions
/// on the acquired connection before executing SQL that references them.
pub async fn register_decimal_functions(
    connection: &mut SqliteConnection,
) -> Result<(), sqlx::Error> {
    let mut handle = connection.lock_handle().await?;
    let sqlite = handle.as_raw_handle().as_ptr();

    register_result(
        unsafe { register_decimal_sum(sqlite) },
        "sdkwork_decimal_sum",
    )?;
    register_result(
        unsafe { register_decimal_order_key(sqlite) },
        "sdkwork_decimal_order_key",
    )?;
    Ok(())
}

fn register_result(code: c_int, function_name: &str) -> Result<(), sqlx::Error> {
    if code == ffi::SQLITE_OK {
        return Ok(());
    }
    Err(sqlx::Error::Configuration(Box::new(io::Error::other(
        format!("failed to register SQLite function {function_name}: code {code}"),
    ))))
}

unsafe fn register_decimal_sum(sqlite: *mut ffi::sqlite3) -> c_int { unsafe {
    ffi::sqlite3_create_function_v2(
        sqlite,
        DECIMAL_SUM_NAME.as_ptr().cast(),
        1,
        ffi::SQLITE_UTF8 | ffi::SQLITE_DETERMINISTIC,
        ptr::null_mut(),
        None,
        Some(decimal_sum_step),
        Some(decimal_sum_final),
        None,
    )
}}

unsafe fn register_decimal_order_key(sqlite: *mut ffi::sqlite3) -> c_int { unsafe {
    ffi::sqlite3_create_function_v2(
        sqlite,
        DECIMAL_ORDER_KEY_NAME.as_ptr().cast(),
        1,
        ffi::SQLITE_UTF8 | ffi::SQLITE_DETERMINISTIC,
        ptr::null_mut(),
        Some(decimal_order_key),
        None,
        None,
        None,
    )
}}

unsafe extern "C" fn decimal_sum_step(
    context: *mut ffi::sqlite3_context,
    argument_count: c_int,
    arguments: *mut *mut ffi::sqlite3_value,
) { unsafe {
    if argument_count != 1 || arguments.is_null() {
        result_error(context, INVALID_ARGUMENT_MESSAGE);
        return;
    }

    let argument = *arguments;
    if ffi::sqlite3_value_type(argument) == ffi::SQLITE_NULL {
        return;
    }

    let state = ffi::sqlite3_aggregate_context(context, size_of::<DecimalSumState>() as c_int)
        .cast::<DecimalSumState>();
    if state.is_null() {
        result_error(context, MEMORY_MESSAGE);
        return;
    }
    if (*state).failed {
        return;
    }

    let Some(value) = decimal_argument(argument) else {
        (*state).failed = true;
        result_error(context, INVALID_ARGUMENT_MESSAGE);
        return;
    };
    let Ok(scaled) = parse_scaled_decimal(value) else {
        (*state).failed = true;
        result_error(context, INVALID_ARGUMENT_MESSAGE);
        return;
    };
    let Some(total) = (*state).scaled.checked_add(scaled) else {
        (*state).failed = true;
        result_error(context, OVERFLOW_MESSAGE);
        return;
    };
    (*state).scaled = total;
}}

unsafe extern "C" fn decimal_sum_final(context: *mut ffi::sqlite3_context) { unsafe {
    let state = ffi::sqlite3_aggregate_context(context, 0).cast::<DecimalSumState>();
    if state.is_null() {
        result_text(context, &format_scaled_decimal(0));
        return;
    }
    if (*state).failed {
        ffi::sqlite3_result_error_code(context, ffi::SQLITE_CONSTRAINT_FUNCTION);
        return;
    }
    result_text(context, &format_scaled_decimal((*state).scaled));
}}

unsafe extern "C" fn decimal_order_key(
    context: *mut ffi::sqlite3_context,
    argument_count: c_int,
    arguments: *mut *mut ffi::sqlite3_value,
) { unsafe {
    if argument_count != 1 || arguments.is_null() {
        result_error(context, INVALID_ARGUMENT_MESSAGE);
        return;
    }
    let argument = *arguments;
    if ffi::sqlite3_value_type(argument) == ffi::SQLITE_NULL {
        ffi::sqlite3_result_null(context);
        return;
    }
    let Some(value) = decimal_argument(argument) else {
        result_error(context, INVALID_ARGUMENT_MESSAGE);
        return;
    };
    let Ok(scaled) = parse_scaled_decimal(value) else {
        result_error(context, INVALID_ARGUMENT_MESSAGE);
        return;
    };

    // Flipping the sign bit maps signed i128 order to unsigned order. A
    // fixed-width decimal encoding then sorts correctly with SQLite BINARY
    // collation without any floating-point conversion.
    let ordered = (scaled as u128) ^ (1_u128 << 127);
    result_text(context, &format!("{ordered:039}"));
}}

unsafe fn decimal_argument<'a>(argument: *mut ffi::sqlite3_value) -> Option<&'a str> { unsafe {
    let value_type = ffi::sqlite3_value_type(argument);
    if value_type != ffi::SQLITE_TEXT && value_type != ffi::SQLITE_INTEGER {
        return None;
    }
    let pointer = ffi::sqlite3_value_text(argument);
    if pointer.is_null() {
        return None;
    }
    let length = ffi::sqlite3_value_bytes(argument);
    if length < 0 {
        return None;
    }
    let bytes = std::slice::from_raw_parts(pointer.cast::<u8>(), length as usize);
    std::str::from_utf8(bytes).ok()
}}

fn parse_scaled_decimal(value: &str) -> Result<i128, ()> {
    if value.is_empty() || value.trim() != value || value.starts_with('+') {
        return Err(());
    }

    let (negative, unsigned) = match value.strip_prefix('-') {
        Some(unsigned) if !unsigned.is_empty() => (true, unsigned),
        Some(_) => return Err(()),
        None => (false, value),
    };
    let mut parts = unsigned.split('.');
    let whole = parts.next().ok_or(())?;
    let fraction = parts.next();
    if parts.next().is_some()
        || whole.is_empty()
        || !whole.bytes().all(|byte| byte.is_ascii_digit())
        || (whole.len() > 1 && whole.starts_with('0'))
    {
        return Err(());
    }

    let fraction = fraction.unwrap_or_default();
    if unsigned.contains('.')
        && (fraction.is_empty()
            || fraction.len() > SCALE as usize
            || !fraction.bytes().all(|byte| byte.is_ascii_digit()))
    {
        return Err(());
    }
    if whole.len().checked_add(fraction.len()).ok_or(())? > MAX_PRECISION {
        return Err(());
    }

    let whole_value = whole.parse::<i128>().map_err(|_| ())?;
    let fraction_value = if fraction.is_empty() {
        0
    } else {
        let parsed = fraction.parse::<i128>().map_err(|_| ())?;
        parsed
            .checked_mul(10_i128.pow(SCALE - fraction.len() as u32))
            .ok_or(())?
    };
    let scaled = whole_value
        .checked_mul(SCALE_FACTOR)
        .and_then(|number| number.checked_add(fraction_value))
        .ok_or(())?;
    if negative && scaled == 0 {
        return Err(());
    }
    if negative {
        scaled.checked_neg().ok_or(())
    } else {
        Ok(scaled)
    }
}

fn format_scaled_decimal(scaled: i128) -> String {
    let sign = if scaled < 0 { "-" } else { "" };
    let absolute = scaled.unsigned_abs();
    let whole = absolute / SCALE_FACTOR as u128;
    let fraction = absolute % SCALE_FACTOR as u128;
    format!("{sign}{whole}.{fraction:012}")
}

unsafe fn result_text(context: *mut ffi::sqlite3_context, value: &str) { unsafe {
    ffi::sqlite3_result_text(
        context,
        value.as_ptr().cast(),
        value.len() as c_int,
        ffi::SQLITE_TRANSIENT(),
    );
}}

unsafe fn result_error(context: *mut ffi::sqlite3_context, message: &[u8]) { unsafe {
    ffi::sqlite3_result_error(context, message.as_ptr().cast(), message.len() as c_int);
}}

#[cfg(test)]
mod tests {
    use super::*;
    use sdkwork_database_config::{DatabaseConfig, DatabaseEngine, DeploymentMode};
    use sqlx::Row;

    #[test]
    fn parses_only_canonical_precision_38_scale_12_decimals() {
        assert_eq!(parse_scaled_decimal("0").unwrap(), 0);
        assert_eq!(parse_scaled_decimal("1.25").unwrap(), 1_250_000_000_000);
        assert_eq!(parse_scaled_decimal("-1.25").unwrap(), -1_250_000_000_000);
        assert_eq!(
            parse_scaled_decimal("9007199254740992.000000000001").unwrap(),
            9_007_199_254_740_992_000_000_000_001
        );

        for invalid in [
            "",
            " 1",
            "1 ",
            "+1",
            "01",
            ".1",
            "1.",
            "-0",
            "1e2",
            "1.0000000000001",
            "999999999999999999999999999.999999999999",
        ] {
            assert!(
                parse_scaled_decimal(invalid).is_err(),
                "{invalid} must be rejected"
            );
        }
    }

    #[test]
    fn fixed_format_preserves_all_twelve_fraction_digits() {
        assert_eq!(format_scaled_decimal(0), "0.000000000000");
        assert_eq!(format_scaled_decimal(1), "0.000000000001");
        assert_eq!(format_scaled_decimal(-1_250_000_000_000), "-1.250000000000");
    }

    #[tokio::test]
    async fn registered_sum_and_order_key_preserve_values_beyond_f64_precision() {
        let config = DatabaseConfig {
            engine: DatabaseEngine::Sqlite,
            url: "sqlite::memory:".to_owned(),
            mode: DeploymentMode::Standalone,
            max_connections: 1,
            ..Default::default()
        };
        let (pool, _) = crate::sqlite::create_sqlite_pool(&config).await.unwrap();
        sqlx::query("CREATE TABLE amount_fact (bucket TEXT NOT NULL, amount TEXT)")
            .execute(&pool)
            .await
            .unwrap();
        for (bucket, amount) in [
            ("lower", "9007199254740992.000000000001"),
            ("lower", "0.000000000009"),
            ("higher", "9007199254740992.000000000011"),
            ("higher", "0.000000000001"),
        ] {
            sqlx::query("INSERT INTO amount_fact (bucket, amount) VALUES (?1, ?2)")
                .bind(bucket)
                .bind(amount)
                .execute(&pool)
                .await
                .unwrap();
        }

        let rows = sqlx::query(
            r#"
            SELECT bucket, sdkwork_decimal_sum(amount) AS total
            FROM amount_fact
            GROUP BY bucket
            ORDER BY sdkwork_decimal_order_key(sdkwork_decimal_sum(amount)) DESC
            "#,
        )
        .fetch_all(&pool)
        .await
        .unwrap();

        assert_eq!(rows[0].get::<String, _>("bucket"), "higher");
        assert_eq!(
            rows[0].get::<String, _>("total"),
            "9007199254740992.000000000012"
        );
        assert_eq!(
            rows[1].get::<String, _>("total"),
            "9007199254740992.000000000010"
        );
    }

    #[tokio::test]
    async fn decimal_sum_rejects_values_already_coerced_to_real() {
        let config = DatabaseConfig {
            engine: DatabaseEngine::Sqlite,
            url: "sqlite::memory:".to_owned(),
            mode: DeploymentMode::Standalone,
            max_connections: 1,
            ..Default::default()
        };
        let (pool, _) = crate::sqlite::create_sqlite_pool(&config).await.unwrap();

        let error = sqlx::query_scalar::<_, String>(
            "SELECT sdkwork_decimal_sum(value) FROM (SELECT CAST(1.25 AS REAL) AS value)",
        )
        .fetch_one(&pool)
        .await
        .expect_err("REAL input must fail instead of silently losing decimal precision");

        assert!(
            error.to_string().contains("invalid canonical decimal"),
            "unexpected error: {error}"
        );
    }
}
