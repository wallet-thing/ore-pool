mod aggregator;
mod contributor;
mod database;
mod error;
mod operator;
mod tx;
mod utils;

use core::panic;

use actix_web::{get, middleware, web, App, HttpResponse, HttpServer, Responder};
use aggregator::{Aggregator, Contribution};
use database::create_pool;
use operator::Operator;
use utils::create_cors;

// TODO: publish attestation to s3
// write attestation url to db with last-hash-at as foreign key
#[actix_web::main]
async fn main() -> Result<(), error::Error> {
    // db connection pool
    let pool = create_pool();
    let pool = web::Data::new(pool);
    // operator and aggregator mutex
    let operator = web::Data::new(Operator::new()?);
    let aggregator = tokio::sync::RwLock::new(Aggregator::new(&operator).await?);
    let aggregator = web::Data::new(aggregator);
    // contributions async channel
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<Contribution>();
    let tx = web::Data::new(tx);
    // env vars
    let attribution_epoch = attribution_epoch()?;

    // aggregate contributions
    tokio::task::spawn({
        log::info!("starting aggregator thread");
        let operator = operator.clone();
        let aggregator = aggregator.clone();
        let pool = pool.clone();
        async move {
            if let Err(err) = aggregator::process_contributions(
                aggregator.as_ref(),
                operator.as_ref(),
                pool.as_ref(),
                &mut rx,
            )
            .await
            {
                log::error!("{:?}", err);
            }
        }
    });

    // kick off attribution loop
    tokio::task::spawn({
        let aggregator = aggregator.clone();
        let operator = operator.clone();
        let pool = pool.clone();
        async move {
            loop {
                // acquire aggregator lock to freeze contributions while submitting attributions
                let lock = aggregator.write().await;
                // submit attributions
                let operator = operator.clone().into_inner();
                if let Err(err) = operator.attribute_members(pool.as_ref()).await {
                    panic!("{:?}", err)
                }
                drop(lock);
                // sleep until next attribution epoch
                tokio::time::sleep(tokio::time::Duration::from_secs(60 * attribution_epoch)).await;
            }
        }
    });

    // launch server
    HttpServer::new(move || {
        env_logger::init();
        log::info!("starting server");
        App::new()
            .wrap(middleware::Logger::default())
            .wrap(create_cors())
            .app_data(pool.clone())
            .app_data(tx.clone())
            .app_data(operator.clone())
            .app_data(aggregator.clone())
            .service(web::resource("/member/{authority}").route(web::get().to(contributor::member)))
            .service(web::resource("/pool-address").route(web::get().to(contributor::pool_address)))
            .service(web::resource("/register").route(web::post().to(contributor::register)))
            .service(web::resource("/contribute").route(web::post().to(contributor::contribute)))
            .service(web::resource("/challenge").route(web::get().to(contributor::challenge)))
            .service(health)
    })
    .bind("0.0.0.0:3000")?
    .run()
    .await
    .map_err(From::from)
}

// denominated in minutes
fn attribution_epoch() -> Result<u64, error::Error> {
    let string = std::env::var("ATTR_EPOCH")?;
    let epoch: u64 = string.parse()?;
    Ok(epoch)
}

#[get("/health")]
async fn health() -> impl Responder {
    HttpResponse::Ok().body("ok")
}
