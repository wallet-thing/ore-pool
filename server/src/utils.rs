use std::env;

use actix_cors::Cors;
use actix_web::http::header;

use crate::error::Error;

pub fn create_cors() -> Cors {
    Cors::default()
        .allowed_origin_fn(|_origin, _req_head| {
            // origin.as_bytes().ends_with(b"ore.supply") || // Production origin
            // origin == "http://localhost:8080" // Local development origin
            true
        })
        .allowed_methods(vec!["GET", "POST"]) // Methods you want to allow
        .allowed_headers(vec![header::AUTHORIZATION, header::ACCEPT])
        .allowed_header(header::CONTENT_TYPE)
        .max_age(3600)
}

pub fn try_env_var(name: &str) -> Result<String, Error> {
    env::var(name).map_err(|e| Error::StdEnv(name.to_string(), e))
}

pub fn env_var_or_panic(name: &str) -> String {
    try_env_var(name).expect(&format!("Required environment variable {} not set", name))
}