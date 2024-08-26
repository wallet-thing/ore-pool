use actix_web::{web, HttpResponse, Responder};
use drillx::Solution;
use serde::{Deserialize, Serialize};
use solana_sdk::{pubkey::Pubkey, signature::Signature};

use crate::{aggregator::Aggregator, Contribution};

/// The payload to send to the /contribute endpoint.
#[derive(Debug, Deserialize, Serialize)]
pub struct ContributePayload {
    /// The authority of the member account sending the payload.
    pub authority: Pubkey,

    /// The solution submitted.
    pub solution: Solution,

    /// Must be a valid signature of the solution
    pub signature: Signature,
}

/// Accepts solutions from pool members. If their solutions are valid, it
/// aggregates the contributions into a list for publishing and submission.
pub async fn contribute(
    payload: web::Json<ContributePayload>,
    tx: web::Data<tokio::sync::mpsc::Sender<Contribution>>,
    aggregator: web::Data<tokio::sync::Mutex<Aggregator>>,
) -> impl Responder {
    // lock aggregrator to ensure we're contributing to the current challenge
    let aggregator = aggregator.as_ref();
    let aggregator = aggregator.lock().await;
    // decode solution difficulty
    let solution = &payload.solution;
    let difficulty = solution.to_hash().difficulty();
    // authenticate the sender signature
    if !payload
        .signature
        .verify(&payload.authority.to_bytes(), &solution.to_bytes())
    {
        return HttpResponse::Unauthorized().finish();
    }
    // error if solution below min difficulty
    if difficulty < (aggregator.challenge.min_difficulty as u32) {
        log::error!("solution below min difficulity: {:?}", payload.authority);
        return HttpResponse::BadRequest().finish();
    }
    // error if digest is invalid
    if !drillx::is_valid_digest(&aggregator.challenge.challenge, &solution.n, &solution.d) {
        return HttpResponse::BadRequest().finish();
    }
    // calculate score
    let score = 2u64.pow(difficulty);
    // TODO: Reject if score is below min difficulty (as defined by the pool operator)
    // update the aggegator
    tx.send(Contribution {
        member: payload.authority,
        score,
        solution: payload.solution,
    })
    .await
    .ok();
    HttpResponse::Ok().finish()
}
