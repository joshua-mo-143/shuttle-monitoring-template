use askama::Template;
use askama_axum::IntoResponse as AskamaIntoResponse;
use axum::{
    extract::{Form, Path, State},
    http::StatusCode,
    response::{IntoResponse as AxumIntoResponse, Redirect, Response},
    routing::{get, post},
    Router,
};
use chrono::Timelike;
use chrono::{DateTime, Utc};
use futures_util::StreamExt;
use reqwest::Client;
use serde::{Deserialize, Serialize};
use sqlx::PgPool;
use tokio::time::{self, Duration};
use validator::Validate;

enum ApiError {
    SQLError(sqlx::Error),
}

impl From<sqlx::Error> for ApiError {
    fn from(e: sqlx::Error) -> Self {
        Self::SQLError(e)
    }
}

impl AxumIntoResponse for ApiError {
    fn into_response(self) -> Response {
        match self {
            Self::SQLError(e) => {
                (
                    StatusCode::INTERNAL_SERVER_ERROR, 
                    format!("SQL Error: {e}")
                    ).into_response()
            }
        }
    }
}

async fn get_websites(State(state): State<AppState>) -> Result<impl AskamaIntoResponse, ApiError> {
    let websites = sqlx::query_as::<_, Website>("SELECT url, alias FROM websites")
        .fetch_all(&state.db)
        .await?;

    let mut logs = Vec::new();

    for website in websites {
        let data = get_daily_stats(&website.alias, &state.db).await?;

        logs.push(WebsiteInfo {
            url: website.url,
            alias: website.alias,
            data,
        });
    }

    Ok(WebsiteLogs { logs })
}

async fn get_daily_stats(alias: &str, db: &PgPool) -> Result<Vec<WebsiteStats>, ApiError> {
    let data = sqlx::query_as::<_, WebsiteStats>(
        r#"
            SELECT date_trunc('hour', created_at) as time, 
            CAST(COUNT(case when status = 200 then 1 end) * 100 / COUNT(*) AS int2) as uptime_pct 
            FROM logs 
            LEFT JOIN websites on websites.id = logs.website_id
            WHERE websites.alias = $1 
            group by time
            order by time asc
            limit 24
            "#,
    )
    .bind(alias)
    .fetch_all(db)
    .await?;

    let number_of_splits = 24;
    let number_of_seconds = 3600;

    let data = fill_data_gaps(data, number_of_splits, SplitBy::Hour, number_of_seconds);

    Ok(data)
}

async fn get_monthly_stats(alias: &str, db: &PgPool) -> Result<Vec<WebsiteStats>, ApiError> {
    let data = sqlx::query_as::<_, WebsiteStats>(
        r#"
            SELECT date_trunc('day', created_at) as time, 
            CAST(COUNT(case when status = 200 then 1 end) * 100 / COUNT(*) AS int2) as uptime_pct 
            FROM logs 
            LEFT JOIN websites on websites.id = logs.website_id
            WHERE websites.alias = $1 
            group by time
            order by time asc
            limit 30
            "#,
    )
    .bind(alias)
    .fetch_all(db)
    .await?;

    let number_of_splits = 30;
    let number_of_seconds = 86400;

    let data = fill_data_gaps(data, number_of_splits, SplitBy::Day, number_of_seconds);
    Ok(data)
}

enum SplitBy {
    Hour,
    Day,
}

fn fill_data_gaps(
    mut data: Vec<WebsiteStats>,
    splits: i32,
    format: SplitBy,
    number_of_seconds: i32,
) -> Vec<WebsiteStats> {
    if (data.len() as i32) < splits {
        for i in 1..24 {
            let time = Utc::now() - chrono::Duration::seconds((number_of_seconds * i).into());
            let time = time
                .with_minute(0)
                .unwrap()
                .with_second(0)
                .unwrap()
                .with_nanosecond(0)
                .unwrap();

            let time = if matches!(format, SplitBy::Day) {
                time.with_hour(0).unwrap()
            } else {
                time
            };

            if !data.iter().any(|x| x.time == time) {
                data.push(WebsiteStats {
                    time,
                    uptime_pct: None,
                });
            }
        }
        data.sort_by(|a, b| b.time.cmp(&a.time));
    }

    data
}

async fn get_website_by_alias(
    State(state): State<AppState>,
    Path(alias): Path<String>,
) -> Result<impl AskamaIntoResponse, ApiError> {
    let website = sqlx::query_as::<_, Website>("SELECT url, alias FROM websites WHERE alias = $1")
        .bind(&alias)
        .fetch_one(&state.db)
        .await?;

    let last_24_hours_data = get_daily_stats(&website.alias, &state.db).await?;
    let monthly_data = get_monthly_stats(&website.alias, &state.db).await?;

    let incidents = sqlx::query_as::<_, Incident>(
        "SELECT logs.created_at as time, logs.status from logs left join websites on websites.id = logs.website_id where websites.alias = $1 and logs.status != 200",
    )
    .bind(&alias)
    .fetch_all(&state.db)
    .await?;

    let log = WebsiteInfo {
        url: website.url,
        alias,
        data: last_24_hours_data,
    };

    Ok(SingleWebsiteLogs {
        log,
        incidents,
        monthly_data,
    })
}

async fn styles() -> impl AxumIntoResponse {
    Response::builder()
        .status(StatusCode::OK)
        .header("Content-Type", "text/css")
        .body(include_str!("../templates/styles.css").to_owned())
        .unwrap()
}

async fn create_website(
    State(state): State<AppState>,
    Form(new_website): Form<Website>,
) -> Result<impl AxumIntoResponse, impl AxumIntoResponse> {
    if new_website.validate().is_err() {
        return Err((
            StatusCode::INTERNAL_SERVER_ERROR,
            "Validation error: is your website a reachable URL?",
        ));
    }

    sqlx::query("INSERT INTO websites (url, alias) VALUES ($1, $2)")
        .bind(new_website.url)
        .bind(new_website.alias)
        .execute(&state.db)
        .await
        .unwrap();

    Ok(Redirect::to("/"))
}

async fn delete_website(
    State(state): State<AppState>,
    Path(alias): Path<String>,
) -> Result<impl AxumIntoResponse, ApiError> {
    let mut tx = state.db.begin().await?;
    if let Err(e) = sqlx::query("DELETE FROM logs WHERE website_alias = $1")
        .bind(&alias)
        .execute(&mut *tx)
        .await {
            tx.rollback().await?;
            return Err(ApiError::SQLError(e));
        };

    if let Err(e) = sqlx::query("DELETE FROM websites WHERE alias = $1")
        .bind(&alias)
        .execute(&mut *tx)
        .await {
            tx.rollback().await?;
            return Err(ApiError::SQLError(e));
        }

    tx.commit().await?;

    Ok(StatusCode::OK)
}

#[derive(Deserialize, sqlx::FromRow, Validate)]
struct Website {
    #[validate(url)]
    url: String,
    alias: String,
}

#[derive(Serialize, sqlx::FromRow, Template)]
#[template(path = "index.html")]
struct WebsiteLogs {
    logs: Vec<WebsiteInfo>,
}

#[derive(Serialize, sqlx::FromRow, Template)]
#[template(path = "single_website.html")]
struct SingleWebsiteLogs {
    log: WebsiteInfo,
    incidents: Vec<Incident>,
    monthly_data: Vec<WebsiteStats>,
}

#[derive(sqlx::FromRow, Serialize)]
pub struct Incident {
    time: DateTime<Utc>,
    status: i16,
}

#[derive(sqlx::FromRow, Serialize)]
pub struct WebsiteStats {
    time: DateTime<Utc>,
    uptime_pct: Option<i16>,
}

#[derive(Serialize, Validate)]
struct WebsiteInfo {
    #[validate(url)]
    url: String,
    alias: String,
    data: Vec<WebsiteStats>,
}

#[derive(Clone)]
struct AppState {
    db: PgPool,
}

impl AppState {
    fn new(db: PgPool) -> Self {
        Self { db }
    }
}

#[shuttle_runtime::main]
async fn main(#[shuttle_shared_db::Postgres] db: PgPool) -> shuttle_axum::ShuttleAxum {
    sqlx::migrate!().run(&db).await.unwrap();

    let state = AppState::new(db.clone());

    tokio::spawn(async move {
        check_websites(db).await;
    });

    let router = Router::new()
        .route("/", get(get_websites))
        .route("/websites", post(create_website))
        .route(
            "/websites/:alias",
            get(get_website_by_alias).delete(delete_website),
        )
        .route("/styles.css", get(styles))
        .with_state(state);

    Ok(router.into())
}

async fn check_websites(db: PgPool) {
    let mut interval = time::interval(Duration::from_secs(60));
    loop {
        interval.tick().await;

        let ctx = Client::new();

        let mut res = sqlx::query_as::<_, Website>("SELECT url, alias FROM websites").fetch(&db);

        while let Some(website) = res.next().await {
            let website = website.unwrap();

            let response = ctx.get(website.url).send().await.unwrap();

            sqlx::query(
                "INSERT INTO logs (website_id, status)
                        VALUES
                        ((SELECT id FROM websites where alias = $1), $2)",
            )
            .bind(website.alias)
            .bind(response.status().as_u16() as i16)
            .execute(&db)
            .await
            .unwrap();
        }
    }
}
