use askama::Template;
use askama_axum::IntoResponse as AskamaIntoResponse;
use axum::{
    extract::{Form, Path, State},
    http::StatusCode,
    response::{IntoResponse as AxumIntoResponse, Redirect, Response},
    routing::{get, post},
    Router,
};
use chrono::{DateTime, Utc};
use futures_util::StreamExt;
use reqwest::Client;
use serde::{Deserialize, Serialize};
use sqlx::PgPool;
use tokio::time::{sleep, Duration};
use validator::Validate;

async fn get_websites(State(state): State<AppState>) -> impl AskamaIntoResponse {
    let websites = sqlx::query_as::<_, Website>("SELECT url, alias FROM websites")
        .fetch_all(&state.db)
        .await
        .unwrap();

    let mut logs = Vec::new();

    for website in websites {
        let mut data = sqlx::query_as::<_, WebsiteStats>(
            r#"
            SELECT date_trunc('hour', created_at) as time, 
            CAST(COUNT(case when status = 200 then 1 end) * 100 / COUNT(*) AS int2) as uptime_pct 
            FROM logs WHERE website_alias = $1 
            group by time
            order by time asc
            limit 24
            "#,
        )
        .bind(&website.alias)
        .fetch_all(&state.db)
        .await
        .unwrap();

        if data.len() < 24 {
            for i in 1..24 {
                let created_at = Utc::now().format("%Y/%m/%d %H:00:00.000 %z").to_string();
                let created_at = DateTime::parse_from_str(&created_at, "%Y/%m/%d %H:%M:%S%.3f %z")
                    .unwrap()
                    - chrono::Duration::seconds((3600 * i).into());

                if !data.iter().any(|x| x.time == created_at) {
                    data.push(WebsiteStats {
                        time: created_at.into(),
                        uptime_pct: None,
                    });
                }
            }
            data.sort_by(|a, b| b.time.cmp(&a.time));
        }

        logs.push(WebsiteInfo {
            url: website.url,
            alias: website.alias,
            data,
        });
    }

    WebsiteLogs { logs }
}

async fn get_website_by_id(
    State(state): State<AppState>,
    Path(alias): Path<String>,
) -> impl AskamaIntoResponse {
    let website = sqlx::query_as::<_, Website>("SELECT url, alias FROM websites WHERE alias = $1")
        .bind(&alias)
        .fetch_one(&state.db)
        .await
        .unwrap();

    let mut data = sqlx::query_as::<_, WebsiteStats>(
        r#"
            SELECT date_trunc('hour', created_at) as time, 
            CAST(COUNT(case when status = 200 then 1 end) * 100 / COUNT(*) AS int2) as uptime_pct 
            FROM logs WHERE website_alias = $1 
            group by time
            order by time asc
            limit 24
            "#,
    )
    .bind(&alias)
    .fetch_all(&state.db)
    .await
    .unwrap();

    if data.len() < 24 {
        for i in 1..24 {
            let created_at = Utc::now().format("%Y/%m/%d %H:00:00.000 %z").to_string();
            let created_at = DateTime::parse_from_str(&created_at, "%Y/%m/%d %H:%M:%S%.3f %z")
                .unwrap()
                - chrono::Duration::seconds((3600 * i).into());

            if !data.iter().any(|x| x.time == created_at) {
                data.push(WebsiteStats {
                    time: created_at.into(),
                    uptime_pct: None,
                });
            }
        }
        data.sort_by(|a, b| b.time.cmp(&a.time));
    }

    let incidents = sqlx::query_as::<_, Incident>(
        "SELECT created_at as time, status from logs where website_alias = $1 and status != 200",
    )
    .bind(&alias)
    .fetch_all(&state.db)
    .await
    .unwrap();

    let log = WebsiteInfo {
        url: website.url,
        alias,
        data,
    };

    SingleWebsiteLogs { log, incidents }
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
) -> Result<impl AxumIntoResponse, impl AxumIntoResponse> {
    if let Err(e) = sqlx::query("DELETE FROM logs WHERE website_alias = $1")
        .bind(&alias)
        .execute(&state.db)
        .await
    {
        return Err((
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("Could not execute SQL query: {e}"),
        ));
    }

    if let Err(e) = sqlx::query("DELETE FROM websites WHERE alias = $1")
        .bind(&alias)
        .execute(&state.db)
        .await
    {
        return Err((
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("Could not execute SQL query: {e}"),
        ));
    }

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
            get(get_website_by_id).delete(delete_website),
        )
        .route("/styles.css", get(styles))
        .with_state(state);

    Ok(router.into())
}

async fn check_websites(db: PgPool) {
    loop {
        let ctx = Client::new();

        let mut res = sqlx::query_as::<_, Website>("SELECT url, alias FROM websites").fetch(&db);

        while let Some(website) = res.next().await {
            let website = website.unwrap();

            let response = ctx.get(website.url).send().await.unwrap();

            sqlx::query(
                "INSERT INTO logs (website_alias, status)
                        VALUES
                        ($1, $2)
                        ON CONFLICT DO NOTHING",
            )
            .bind(website.alias)
            .bind(response.status().as_u16() as i16)
            .execute(&db)
            .await
            .unwrap();
        }

        // We want to request each website once a minute - we add 2 seconds
        // The default stored value in Postgres is truncated to once per minute
        let next_time = Utc::now().format("%Y/%m/%d %H:%M:02.000 %z").to_string();
        let next_time = DateTime::parse_from_str(&next_time, "%Y/%m/%d %H:%M:%S%.3f %z").unwrap()
            + chrono::Duration::seconds(60);

        let duration_to_wait = next_time.signed_duration_since(Utc::now()).num_seconds() as u64;

        sleep(Duration::from_secs(duration_to_wait)).await;
    }
}
