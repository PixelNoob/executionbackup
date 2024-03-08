use axum::{
    self,
    extract::{self, DefaultBodyLimit},
    http::{header, HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    Extension, Router,
};
use ethereum_types::{H256, U256};
use futures::future::join_all;

use serde_json::json;
use std::{any::type_name, collections::HashMap, net::SocketAddr, sync::Arc};
use tokio::{
    sync::{Mutex, RwLock},
    time::Duration,
};
use tracing_subscriber::filter::EnvFilter;
mod verify_hash;
use regex::Regex;
use types::{node::Node, *};
use verify_hash::verify_payload_block_hash;

const VERSION: &str = "1.2.0";

pub fn fork_name_at_epoch(epoch: u64, fork_config: &ForkConfig) -> ForkName {
    if let Some(fork_epoch) = fork_config.cancun_fork_epoch {
        if epoch >= fork_epoch {
            return ForkName::Cancun;
        }
    }
    if let Some(fork_epoch) = fork_config.shanghai_fork_epoch {
        if epoch >= fork_epoch {
            return ForkName::Shanghai;
        }
    }
    ForkName::Merge
}

fn timestamp_to_version(timestamp: &u64, fork_config: &ForkConfig) -> Option<ForkName> {
    // 32 slots/epoch
    let slot = timestamp.checked_sub(1606824000)?.checked_div(12)?; // genesis time / seconds per slot
    let epoch = slot.checked_div(32)?; // slot / slots per epoch
    Some(fork_name_at_epoch(epoch, fork_config))
}

pub fn newpayload_serializer(
    mut request: RpcRequest,
    fork_config: &ForkConfig,
) -> Result<NewPayloadRequest, String> {
    let params = match request.params.as_array_mut() {
        Some(params_vec) => params_vec,
        None => {
            tracing::error!("Could not serialize newPayload's params into a vec.");
            return Err("Could not serialize newPayload's params into a vec".to_string());
        }
    };

    if request.method == EngineMethod::engine_newPayloadV3 {
        // params will have 3 fields: [ExecutionPayloadV3, expectedBlobVersionedHashes, ParentBeaconBlockRoot]
        if params.len() != 3 {
            tracing::error!("newPayloadV3's params did not have 3 elements.");
            return Err("newPayloadV3's params did not have 3 elements.".to_string());
        }

        let execution_payload: ExecutionPayloadV3 = match serde_json::from_value(params[0].take()) {
            // direct getting is safe here since we checked that we have least 3 elements
            Ok(execution_payload) => execution_payload,
            Err(e) => {
                tracing::error!(
                    "Could not serialize ExecutionPayload from newPayloadV3: {}",
                    e
                );
                return Err("Could not serialize ExecutionPayload".to_string());
            }
        };

        let versioned_hashes: Vec<H256> = match serde_json::from_value(params[1].take()) {
            Ok(versioned_hashes) => versioned_hashes,
            Err(e) => {
                tracing::error!(
                    "Could not serialize VersionedHashes from newPayloadV3: {}",
                    e
                );
                return Err("Could not serialize Versioned Hashes.".to_string());
            }
        };

        let parent_beacon_block_root: H256 = match serde_json::from_value(params[2].take()) {
            Ok(parent_beacon_block_root) => parent_beacon_block_root,
            Err(e) => {
                tracing::error!(
                    "Could not serialize ParentBeaconBlockRoot from newPayloadV3: {}",
                    e
                );
                return Err("Could not serialize ParentBeaconBlockRoot.".to_string());
            }
        };

        return Ok(NewPayloadRequest {
            execution_payload: types::ExecutionPayload::V3(execution_payload),
            expected_blob_versioned_hashes: Some(versioned_hashes),
            parent_beacon_block_root: Some(parent_beacon_block_root),
        });
    }

    // parmas will just have [ExecutionPayloadV1 | ExecutionPayloadV2]

    if params.len() != 1 {
        tracing::error!("newPayloadV1|2's params did not have anything or something went wrong (newPayloadV1|2 called with more than just 1 param (ExecutionPayload).");
        return Err("newPayloadV1|2's params did not have anything.".to_string());
    }

    let QuantityU64 { value: timestamp } = match params[0].get("timestamp") {
        Some(timestamp) => {
            match serde_json::from_value(timestamp.clone()) {
                Ok(timestamp) => timestamp,
                Err(e) => {
                    tracing::error!("Execution payload timestamp is not representable as u64: {}. Timestamp: {}", e, timestamp);
                    return Err(
                        "Execution payload timestamp is not representable as u64".to_string()
                    );
                }
            }
        }
        None => {
            tracing::error!("Execution payload does not have timestamp");
            return Err("Execution payload does not have timestamp".to_string());
        }
    };

    let fork_name = match timestamp_to_version(&timestamp, fork_config) {
        Some(fork_name) => fork_name,
        None => {
            tracing::error!("Error converting execution payload timestamp to fork name");
            return Err("Error converting execution payload timestamp to fork name".to_string());
        }
    };

    let execution_payload = match fork_name {
        ForkName::Merge => match serde_json::from_value::<ExecutionPayloadV1>(params[0].take()) {
            Ok(execution_payload) => ExecutionPayload::V1(execution_payload),
            Err(e) => {
                tracing::error!(
                        "Could not serialize ExecutionPayloadV1 from newPayloadV1|2; Merge fork. Error: {}",
                        e
                    );
                return Err("Could not serialize ExecutionPayload.".to_string());
            }
        },
        ForkName::Shanghai => {
            match serde_json::from_value::<ExecutionPayloadV2>(params[0].take()) {
                Ok(execution_payload) => ExecutionPayload::V2(execution_payload),
                Err(e) => {
                    tracing::error!(
                        "Could not serialize ExecutionPayloadV2 from newPayloadV2; Shanghai fork. Error: {}",
                        e
                    );
                    return Err("Could not serialize ExecutionPayload.".to_string());
                }
            }
        }
        ForkName::Cancun => match serde_json::from_value::<ExecutionPayloadV3>(params[0].take()) {
            Ok(execution_payload) => ExecutionPayload::V3(execution_payload),
            Err(e) => {
                tracing::error!(
                        "Could not serialize ExecutionPayloadV3 from newPayloadV3; Cancun fork. Error: {}",
                        e
                    );
                return Err("Could not serialize ExecutionPayload.".to_string());
            }
        },
    };

    Ok(NewPayloadRequest {
        execution_payload,
        expected_blob_versioned_hashes: None,
        parent_beacon_block_root: None,
    })
}

fn make_response(id: &u64, result: serde_json::Value) -> String {
    json!({"jsonrpc":"2.0","id":id,"result":result}).to_string()
}

fn make_error(id: &u64, error: &str) -> String {
    json!({"jsonrpc": "2.0", "id": id, "error": {"code": -32700, "message": error}}).to_string()
}

fn parse_result(resp: &str) -> Result<serde_json::Value, ParseError> {
    let j = match serde_json::from_str::<serde_json::Value>(resp) {
        Ok(j) => j,
        Err(e) => {
            tracing::error!(reponse_body = ?resp, "Error deserializing response: {}", e);
            return Err(ParseError::InvalidJson);
        }
    };

    if let Some(error) = j.get("error") {
        tracing::error!(reponse_body = ?resp, "Response has error: {}", error);
        return Err(ParseError::ElError);
    }

    let result = match j.get("result") {
        Some(result) => result,
        None => {
            tracing::error!(reponse_body = ?resp, "Response has no result field");
            return Err(ParseError::MethodNotFound);
        }
    };

    Ok(result.clone())
}

fn make_syncing_str(
    id: &u64,
    payload: &serde_json::Value,
    method: &EngineMethod,
    parent_beacon_block_root: Option<H256>,
) -> String {
    match method {
        EngineMethod::engine_newPayloadV1 | EngineMethod::engine_newPayloadV2 | EngineMethod::engine_newPayloadV3 => {
            tracing::debug!(
                "Verifying execution payload blockhash {}.",
                payload["blockHash"]
            );

            let execution_payload = match method {
                EngineMethod::engine_newPayloadV1 => match serde_json::from_value::<ExecutionPayloadV1>(payload.clone()) {
                        Ok(execution_payload) => ExecutionPayload::V1(execution_payload),
                        Err(e) => {
                            tracing::error!("Error deserializing execution payload: {}", e);
                            return e.to_string();
                        }
                    },

                EngineMethod::engine_newPayloadV2 => match serde_json::from_value::<ExecutionPayloadV2>(payload.clone()) {
                        Ok(execution_payload) => ExecutionPayload::V2(execution_payload),
                        Err(e) => {
                            tracing::error!("Error deserializing execution payload: {}", e);
                            return e.to_string();
                        }
                    },

                EngineMethod::engine_newPayloadV3 => match serde_json::from_value::<ExecutionPayloadV3>(payload.clone()) {
                        Ok(execution_payload) => ExecutionPayload::V3(execution_payload),
                        Err(e) => {
                            tracing::error!("Error deserializing execution payload: {}", e);
                            return e.to_string();
                        }
                    },
                _ => unreachable!("File a issue on Github. This should never happen. Matched non-newPayload inside previously matched newPayload"),
            };

            if let Err(e) = verify_payload_block_hash(&execution_payload, parent_beacon_block_root) {
                tracing::error!("Error verifying execution payload blockhash: {}", e);
                return e.to_string();
            }

            tracing::debug!(
                "Execution payload blockhash {} verified. Returning SYNCING",
                payload["blockHash"]
            );
            json!({"result":{"latestValidHash":null,"status":"SYNCING","validationError":null},"id":id,"jsonrpc":"2.0"}).to_string()
        },

        EngineMethod::engine_forkchoiceUpdatedV1 | EngineMethod::engine_forkchoiceUpdatedV2 | EngineMethod::engine_forkchoiceUpdatedV3 => {
            json!({"jsonrpc":"2.0","id":id,"result":{"payloadStatus":{"status":"SYNCING","latestValidHash":null,"validationError":null}},"payloadId":null}).to_string()
        },

        _ => {
            "Called make_syncing_str with a non fcu or newpayload request".to_string()
        }
    }
}

struct NodeRouter {
    nodes: Arc<Mutex<Vec<Arc<Node>>>>,
    alive_nodes: Arc<RwLock<Vec<Arc<Node>>>>,
    dead_nodes: Arc<RwLock<Vec<Arc<Node>>>>,
    alive_but_syncing_nodes: Arc<RwLock<Vec<Arc<Node>>>>,

    // this node will be the selected primary node used to route all requests
    primary_node: Arc<RwLock<Arc<Node>>>,

    // jwt encoded key used to make tokens for the EE's auth port
    // jwt_key: Arc<jsonwebtoken::EncodingKey>,

    // percentage of nodes that need to agree for it to be deemed a majority
    majority_percentage: f32, // 0.1..0.9

    // setting to set if node timings are displayed
    node_timings_enabled: bool,

    fork_config: ForkConfig,

    // for if we want to use a general jwt with /create_node
    general_jwt: Option<jsonwebtoken::EncodingKey>,
}

impl NodeRouter {
    fn new(
        //jwt_key: &jsonwebtoken::EncodingKey,
        majority_percentage: f32,
        nodes: Vec<Arc<Node>>,
        primary_node: Arc<Node>,
        node_timings_enabled: bool,
        fork_config: ForkConfig,
        general_jwt: Option<jsonwebtoken::EncodingKey>,
    ) -> Self {
        NodeRouter {
            nodes: Arc::new(Mutex::new(nodes.clone())),
            alive_nodes: Arc::new(RwLock::new(Vec::new())),
            dead_nodes: Arc::new(RwLock::new(Vec::new())),
            alive_but_syncing_nodes: Arc::new(RwLock::new(Vec::new())),
            primary_node: Arc::new(RwLock::new(primary_node)),
            //jwt_key: Arc::new(jwt_key.clone()),
            majority_percentage,
            node_timings_enabled,
            fork_config,
            general_jwt,
        }
    }

    async fn make_node_syncing(&self, node: Arc<Node>) {
        let mut alive_nodes = self.alive_nodes.write().await;
        let index = alive_nodes.iter().position(|x| *x.url == node.url);
        let index = match index {
            Some(index) => index,
            None => {
                // node is not in alive_nodes, so it's in syncing or worse, stop here
                return;
            }
        };

        let mut alive_but_syncing_nodes = self.alive_but_syncing_nodes.write().await;
        alive_nodes.remove(index);
        alive_but_syncing_nodes.push(node.clone());
        node.set_online_and_syncing().await;
    }

    // returns Vec<T> where it tries to deserialize for each resp to T
    async fn concurrent_requests<T>(&self, request: &RpcRequest, jwt_token: String) -> Vec<T>
    where
        T: serde::de::DeserializeOwned,
    {
        let alive_nodes = self.alive_nodes.read().await;
        let mut futs = Vec::with_capacity(alive_nodes.len());

        alive_nodes
            .iter()
            .for_each(|node| futs.push(node.do_request(request, jwt_token.clone())));

        let mut out = Vec::with_capacity(alive_nodes.len());
        let completed = join_all(futs).await;
        drop(alive_nodes);

        for resp in completed {
            match resp {
                Ok(resp) => {
                    // response from node
                    let result = match parse_result(&resp.0) {
                        Ok(result) => result,
                        Err(e) => {
                            tracing::error!(
                                "Couldn't parse node result for {:?}: {:?}",
                                request.method,
                                e
                            );
                            continue;
                        }
                    };

                    match serde_json::from_value::<T>(result) {
                        Ok(deserialized) => {
                            out.push(deserialized);
                        }
                        Err(e) => {
                            tracing::error!(
                                "Couldn't deserialize response {:?} from node to type {}: {}",
                                request.method,
                                type_name::<T>(),
                                e
                            );
                        }
                    }
                }
                Err(e) => {
                    tracing::error!("{:?} error: {}", request.method, e);
                }
            }
        }

        out
    }

    async fn recheck(&self) {
        // check the status of all nodes
        // order nodes in alive_nodes vector by response time
        // dont clone nodes, just clone the Arcs

        let nodes = self.nodes.lock().await;
        let mut new_alive_nodes = Vec::<(u128, Arc<Node>)>::with_capacity(nodes.len()); // resp time, node
        let mut new_dead_nodes = Vec::<Arc<Node>>::with_capacity(nodes.len());
        let mut new_alive_but_syncing_nodes = Vec::<Arc<Node>>::with_capacity(nodes.len());

        let mut checks = Vec::new();

        for node in nodes.iter() {
            let check = async move {
                match node.check_status().await {
                    Ok(status) => (status, node.clone()),
                    Err(e) => {
                        if e.is_decode() {
                            tracing::error!(
                                "Error while checking node {}: {}; Maybe jwt related?",
                                node.url,
                                e
                            );
                        } else {
                            tracing::error!("Error while checking node {}: {}", node.url, e);
                        }

                        (
                            NodeHealth {
                                status: SyncingStatus::Offline,
                                resp_time: 0,
                            },
                            node.clone(),
                        )
                    }
                }
            };
            checks.push(check);
        }

        let results = join_all(checks).await;

        for (status, node) in results {
            if status.status == SyncingStatus::Synced {
                new_alive_nodes.push((status.resp_time, node.clone()));

                if self.node_timings_enabled {
                    tracing::info!("{}: {:.2}ms", node.url, (status.resp_time as f64 / 1000.0));
                    // resp_time is in micros
                }
            } else if status.status == SyncingStatus::OnlineAndSyncing {
                new_alive_but_syncing_nodes.push(node.clone());

                if self.node_timings_enabled {
                    tracing::info!("{}: {:.2}ms", node.url, (status.resp_time as f64 / 1000.0));
                }
            } else {
                new_dead_nodes.push(node.clone());
                if self.node_timings_enabled {
                    tracing::warn!("Dead node: {}", node.url);
                }
            }
        }

        // sort alive_nodes by response time
        new_alive_nodes.sort_by(|a, b| a.0.cmp(&b.0));

        // update primary node to be the first alive node
        let mut primary_node = self.primary_node.write().await;
        *primary_node = match new_alive_nodes.first() {
            Some(node) => node.1.clone(),
            None => {
                // if there are no alive nodes, then set the primary node to a syncing node
                match new_alive_but_syncing_nodes.first() {
                    Some(node) => node.clone(),
                    None => {
                        // if there are no syncing nodes, then set the primary node to a dead node
                        match new_dead_nodes.first() {
                            Some(node) => node.clone(),
                            None => {
                                // if there are no dead nodes, then set the primary node to the first node
                                nodes[0].clone()
                            }
                        }
                    }
                }
            }
        };
        drop(primary_node);

        // lock alive_nodes, dead_nodes, and alive_but_syncing_nodes
        let mut alive_but_syncing_nodes = self.alive_but_syncing_nodes.write().await; // we have a hard time acquiring this lock for some reason
        let mut alive_nodes = self.alive_nodes.write().await;
        let mut dead_nodes = self.dead_nodes.write().await;

        // clear vectors and for alive nodes put the Arc<Node> in the vector
        alive_nodes.clear();
        dead_nodes.clear();
        alive_but_syncing_nodes.clear();

        for (_, node) in new_alive_nodes.iter() {
            alive_nodes.push(node.clone());
        }

        for node in new_dead_nodes.iter() {
            dead_nodes.push(node.clone());
        }

        for node in new_alive_but_syncing_nodes.iter() {
            alive_but_syncing_nodes.push(node.clone());
        }
    }

    // try and return the primary node asap
    // if the primary node is offline, then we'll get the next node in the vector, and set the primary node to that node (if its online)
    // basically, return the node closest to the start of the vector that is online, and set that as the primary node
    // if there are no online nodes, try to use a syncing node
    // if there are no syncing nodes, return None
    async fn get_execution_node(&self) -> Option<Arc<Node>> {
        let primary_node = self.primary_node.read().await;

        if primary_node.status.read().await.status == SyncingStatus::Synced {
            return Some(primary_node.clone());
        }

        let old_primary_node_url = primary_node.url.clone(); // we're going to change it
        drop(primary_node);

        let alive_nodes = self.alive_nodes.read().await;

        if alive_nodes.is_empty() {
            let alive_but_syncing_nodes = self.alive_but_syncing_nodes.read().await;
            if alive_but_syncing_nodes.is_empty() {
                None
            } else {
                // no synced nodes, but there are syncing nodes, so return the first syncing node

                let node = alive_but_syncing_nodes[0].clone();
                let mut primary_node = self.primary_node.write().await;
                *primary_node = node.clone();
                Some(node)
            }
        } else {
            // there are synced nodes, so return the synced node (making sure its not the already checked primary node)
            for node in alive_nodes.iter() {
                if node.url != old_primary_node_url {
                    let node = node.clone();
                    let mut primary_node = self.primary_node.write().await;
                    *primary_node = node.clone();
                    return Some(node);
                }
            }
            // no synced nodes that are not the primary node, so return a syncing node
            let alive_but_syncing_nodes = self.alive_but_syncing_nodes.read().await;
            if alive_but_syncing_nodes.is_empty() {
                // no synced or syncing nodes, so return None
                None
            } else {
                // no synced nodes, but there are syncing nodes, so return the first syncing node

                let node = alive_but_syncing_nodes[0].clone();
                let mut primary_node = self.primary_node.write().await;
                *primary_node = node.clone();
                Some(node)
            }
        }
    }

    // gets the majority response from a vector of respon   ses
    // must have at least majority_percentage of the nodes agree
    // if there is no majority, then return None
    // if there is a draw, just return the first response
    // u64 on the response should be the "id" field from the any of the responses
    fn fcu_majority(&self, results: &Vec<PayloadStatusV1>) -> Option<PayloadStatusV1> {
        let total_responses = results.len();
        let majority_count = (total_responses as f32 * self.majority_percentage) as usize;

        // Create a hashmap to store response frequencies
        let mut response_counts: HashMap<&PayloadStatusV1, usize> = HashMap::new();

        for response in results.iter() {
            *response_counts.entry(response).or_insert(0) += 1;
        }

        // Find the response with the most occurrences
        let mut majority_response = None;
        let mut max_count = 0;

        for (response, &count) in response_counts.iter() {
            if count > max_count {
                majority_response = Some(response);
                max_count = count;
            }
        }

        // Check if the majority count is greater than or equal to the required count
        if max_count >= majority_count {
            majority_response.cloned().cloned()
        } else {
            None
        }
    }

    async fn fcu_logic(
        &self,
        resps: &Vec<PayloadStatusV1>,
        req: &RpcRequest,
        jwt_token: String,
    ) -> Result<PayloadStatusV1, FcuLogicError> {
        if resps.is_empty() {
            // no responses, so return SYNCING
            tracing::error!("No responses, returning SYNCING.");
            return Err(FcuLogicError::NoResponses);
        }

        let majority = match self.fcu_majority(resps) {
            Some(majority) => majority,
            None => {
                // no majority, so return SYNCING
                tracing::error!("No majority, returning SYNCING.");
                return Err(FcuLogicError::NoMajority);
            }
        };

        match majority.status {
            PayloadStatusV1Status::Invalid | PayloadStatusV1Status::InvalidBlockHash => {
                // majority is INVALID, so return INVALID (to not go through the next parts of the algorithm)
                return Ok(majority); // return Ok since this is not an error
            }
            _ => {} // there still can be invalid in the responses
        }

        for resp in resps {
            // check if any of the responses are INVALID

            match resp.status {
                PayloadStatusV1Status::Invalid | PayloadStatusV1Status::InvalidBlockHash => {
                    // a response is INVALID. One node could be right, no risks, return syncing to stall CL
                    return Err(FcuLogicError::OneNodeIsInvalid);
                }
                _ => {}
            }
        }

        // send to the syncing nodes to help them catch up with tokio::spawn so we don't have to wait for them
        let syncing_nodes = self.alive_but_syncing_nodes.clone();
        let jwt_token_clone = jwt_token.clone();
        let req_clone = req.clone();
        tokio::spawn(async move {
            let syncing_nodes = syncing_nodes.read().await.clone();
            tracing::debug!(
                "Sending fcU or newPayload to {} syncing nodes",
                syncing_nodes.len()
            );

            join_all(
                syncing_nodes
                    .iter()
                    .map(|node| node.do_request_no_timeout(&req_clone, jwt_token_clone.clone())),
            )
            .await;
        });

        // majority is checked and either VALID or SYNCING
        Ok(majority)
    }

    async fn do_engine_route(
        &self,
        fork_config: &ForkConfig,
        request: &RpcRequest,
        jwt_token: String,
    ) -> (String, u16) {
        match request.method {
            // getPayloadV1 is for getting a block to be proposed, so no use in getting from multiple nodes
            EngineMethod::engine_getPayloadV1 => {
                let node = match self.get_execution_node().await {
                    None => {
                        return (make_error(&request.id, "No nodes available"), 500);
                    }
                    Some(node) => node,
                };

                let resp = node.do_request_no_timeout(request, jwt_token).await; // no timeout since the CL will just time us out themselves
                tracing::debug!("engine_getPayloadV1 sent to node: {}", node.url);
                match resp {
                    Ok(resp) => (resp.0, resp.1),
                    Err(e) => {
                        tracing::warn!("engine_getPayloadV1 error: {}", e);

                        if e.is_connect() || e.is_timeout() || e.is_request() {
                            // if the error is a connection error, then we should set the node to syncing
                            self.make_node_syncing(node.clone()).await;
                        }

                        (make_error(&request.id, &e.to_string()), 200)
                    }
                }
            } // getPayloadV1

            EngineMethod::engine_getPayloadV2 => {
                // getPayloadV2 has a different schema, where alongside the executionPayload it has a blockValue
                // so we should send this to all the nodes and then return the one with the highest blockValue

                // WILLNOTFIX the spec require getPayloadV2 to support getPayloadResponseV1, but it adds too much complexity
                // for little benefit, as I doubt people actually use getPayloadResponseV2 with getPayloadV2
                let resps: Vec<getPayloadResponseV2> =
                    self.concurrent_requests(request, jwt_token).await;
                let most_profitable = resps
                    .iter()
                    .max_by(|resp_a, resp_b| resp_a.block_value.cmp(&resp_b.block_value));

                if let Some(most_profitable_payload) = most_profitable {
                    tracing::info!("Block {} requested by CL. All EL blocks profitability: {:?}. Using payload with value of {}", most_profitable_payload.execution_payload.block_number, resps.iter().map(|payload| payload.block_value).collect::<Vec<U256>>(), most_profitable_payload.block_value);
                    return (
                        make_response(&request.id, json!(most_profitable_payload)),
                        200,
                    );
                }

                // we have no payloads
                tracing::warn!("No blocks found in EL engine_getPayloadV2 responses");
                (
                    make_error(
                        &request.id,
                        "No blocks found in EL engine_getPayloadV2 responses",
                    ),
                    200,
                )
            } // getPayloadV2

            EngineMethod::engine_getPayloadV3 => {
                // accepts only getPayloadResponseV3 since this version actually modifies the getPayload response (adding blob_bundle)
                // as well as the nested execution payload

                let resps: Vec<getPayloadResponseV3> =
                    self.concurrent_requests(request, jwt_token).await;
                let most_profitable = resps
                    .iter()
                    .max_by(|resp_a, resp_b| resp_a.block_value.cmp(&resp_b.block_value));

                // note: we may want to get the most profitable block from resps that have should_override_builder = true, note this in release

                if let Some(most_profitable_payload) = most_profitable {
                    tracing::info!("Block {} requested by CL. All EL blocks profitability: {:?}. Using payload with value of {}", most_profitable_payload.execution_payload.block_number, resps.iter().map(|payload| payload.block_value).collect::<Vec<U256>>(), most_profitable_payload.block_value);
                    return (
                        make_response(&request.id, json!(most_profitable_payload)),
                        200,
                    );
                }

                // we have no payloads
                tracing::warn!("No blocks found in EL engine_getPayloadV3 responses");
                (
                    make_error(
                        &request.id,
                        "No blocks found in EL engine_getPayloadV2 responses",
                    ),
                    200,
                )
            } // getPayloadV3

            EngineMethod::engine_newPayloadV1 | EngineMethod::engine_newPayloadV2 => {
                tracing::debug!("Sending newPayloadV1|V2 to alive nodes");
                let resps: Vec<PayloadStatusV1> =
                    self.concurrent_requests(request, jwt_token.clone()).await;

                let resp = match self.fcu_logic(&resps, request, jwt_token).await {
                    Ok(resp) => resp,
                    Err(e) => match e {
                        FcuLogicError::NoResponses => {
                            tracing::error!(
                                "No responses for {:?}, returning SYNCING",
                                request.method
                            );
                            return (
                                make_syncing_str(
                                    &request.id,
                                    &request.params[0],
                                    &request.method,
                                    None,
                                ),
                                200,
                            );
                        }
                        FcuLogicError::NoMajority => {
                            tracing::error!(
                                "No majority for {:?}, returning SYNCING",
                                request.method
                            );
                            return (
                                make_syncing_str(
                                    &request.id,
                                    &request.params[0],
                                    &request.method,
                                    None,
                                ),
                                200,
                            );
                        }
                        FcuLogicError::OneNodeIsInvalid => {
                            tracing::error!(
                                "One node is invalid for {:?}, returning SYNCING",
                                request.method
                            );
                            return (
                                make_syncing_str(
                                    &request.id,
                                    &request.params[0],
                                    &request.method,
                                    None,
                                ),
                                200,
                            );
                        }
                    },
                };

                // we have a majority
                (make_response(&request.id, json!(resp)), 200)
            } // newPayloadV1, V2

            EngineMethod::engine_newPayloadV3 => {
                let newpayload_request = match newpayload_serializer(request.clone(), fork_config) {
                    Ok(newpayload_request) => newpayload_request,
                    Err(e) => {
                        tracing::error!("Failed to serialize newPayloadV3: {}", e);
                        return ("Failed to serialize newPayloadV3".to_string(), 500);
                    }
                };

                tracing::debug!("Sending newPayloadV3 to alive nodes");
                let resps: Vec<PayloadStatusV1> =
                    self.concurrent_requests(request, jwt_token.clone()).await;

                let resp = match self.fcu_logic(&resps, request, jwt_token).await {
                    Ok(resp) => resp,
                    Err(e) => match e {
                        FcuLogicError::NoResponses => {
                            tracing::error!(
                                "No responses for {:?}, returning SYNCING",
                                request.method
                            );
                            return (
                                make_syncing_str(
                                    &request.id,
                                    &request.params[0],
                                    &request.method,
                                    newpayload_request.parent_beacon_block_root,
                                ),
                                200,
                            );
                        }
                        FcuLogicError::NoMajority => {
                            tracing::error!(
                                "No majority for {:?}, returning SYNCING",
                                request.method
                            );
                            return (
                                make_syncing_str(
                                    &request.id,
                                    &request.params[0],
                                    &request.method,
                                    newpayload_request.parent_beacon_block_root,
                                ),
                                200,
                            );
                        }
                        FcuLogicError::OneNodeIsInvalid => {
                            tracing::error!(
                                "One node is invalid for {:?}, returning SYNCING",
                                request.method
                            );
                            return (
                                make_syncing_str(
                                    &request.id,
                                    &request.params[0],
                                    &request.method,
                                    newpayload_request.parent_beacon_block_root,
                                ),
                                200,
                            );
                        }
                    },
                };

                // we have a majority
                (make_response(&request.id, json!(resp)), 200)
            } // newPayloadV3

            EngineMethod::engine_forkchoiceUpdatedV1
            | EngineMethod::engine_forkchoiceUpdatedV2
            | EngineMethod::engine_forkchoiceUpdatedV3 => {
                tracing::debug!("Sending fcU to alive nodes");
                let resps: Vec<forkchoiceUpdatedResponse> =
                    self.concurrent_requests(request, jwt_token.clone()).await;

                let mut payloadstatus_resps = Vec::<PayloadStatusV1>::with_capacity(resps.len()); // faster to allocate in one go
                let mut payload_id: Option<String> = None;

                for resp in resps {
                    if let Some(inner_payload_id) = resp.payloadId {
                        // todo: make this look cleaner.
                        payload_id = Some(inner_payload_id); // if payloadId is not null, then use that. all resps will have the same payloadId
                    };
                    payloadstatus_resps.push(resp.payloadStatus);
                }

                let resp = match self
                    .fcu_logic(&payloadstatus_resps, request, jwt_token)
                    .await
                {
                    Ok(resp) => resp,
                    Err(e) => match e {
                        FcuLogicError::NoResponses => {
                            tracing::error!(
                                "No responses for {:?}, returning SYNCING",
                                request.method
                            );
                            return (
                                make_syncing_str(
                                    &request.id,
                                    &request.params[0],
                                    &request.method,
                                    None,
                                ),
                                200,
                            );
                        }
                        FcuLogicError::NoMajority => {
                            tracing::error!(
                                "No majority for {:?}, returning SYNCING",
                                request.method
                            );
                            return (
                                make_syncing_str(
                                    &request.id,
                                    &request.params[0],
                                    &request.method,
                                    None,
                                ),
                                200,
                            );
                        }
                        FcuLogicError::OneNodeIsInvalid => {
                            tracing::error!(
                                "One node is invalid for {:?}, returning SYNCING",
                                request.method
                            );
                            return (
                                make_syncing_str(
                                    &request.id,
                                    &request.params[0],
                                    &request.method,
                                    None,
                                ),
                                200,
                            );
                        }
                    },
                };

                // we have a majority
                (
                    make_response(
                        &request.id,
                        json!(forkchoiceUpdatedResponse {
                            payloadStatus: resp,
                            payloadId: payload_id,
                        }),
                    ),
                    200,
                )
            } // fcU V1, V2

            EngineMethod::engine_getClientVersionV1 => {
                let resps: Vec<serde_json::Value> = self.concurrent_requests(request, jwt_token).await;
                (make_response(&request.id, json!(resps)), 200)
            }

            _ => {
                // wait for primary node's response, but also send to all other nodes
                let primary_node = match self.get_execution_node().await {
                    Some(primary_node) => primary_node,
                    None => {
                        tracing::warn!("No primary node available");
                        return (make_error(&request.id, "No nodes available"), 500);
                    }
                };

                let resp = primary_node
                    .do_request_no_timeout(request, jwt_token.clone())
                    .await;

                // spawn a new task to replicate requests
                let alive_nodes = self.alive_nodes.clone();
                let jwt_token = jwt_token.to_owned();
                let request_clone = request.clone();
                tokio::spawn(async move {
                    let alive_nodes = alive_nodes.read().await.clone();

                    join_all(
                        alive_nodes
                            .iter()
                            .filter(|node| node.url != primary_node.url)
                            .map(|node| {
                                node.do_request_no_timeout(&request_clone, jwt_token.clone())
                            }),
                    )
                    .await;
                });

                // return resp from primary node
                match resp {
                    Ok(resp) => (resp.0, resp.1),
                    Err(e) => {
                        tracing::warn!("Error from primary node: {}", e);
                        (make_error(&request.id, &e.to_string()), 200)
                    }
                }
            } // all other engine requests
        }
    }

    async fn do_route_normal(&self, request: String, jwt_token: String) -> (String, u16) {
        // simply send request to primary node
        let primary_node = match self.get_execution_node().await {
            Some(primary_node) => primary_node,
            None => {
                tracing::warn!("No primary node available for normal request");
                let id = match serde_json::from_str::<RpcRequest>(&request) {
                    Ok(request) => request.id,
                    Err(e) => {
                        tracing::error!("Error deserializing request: {}", e);
                        return (make_error(&0, &e.to_string()), 200);
                    }
                };
                return (make_error(&id, "No nodes available"), 500);
            }
        };

        let resp = primary_node
            .do_request_no_timeout_str(request, jwt_token)
            .await;
        match resp {
            Ok(resp) => (resp.0, resp.1),
            Err(e) => (make_error(&1, &e.to_string()), 200),
        }
    }
}

// func to take body and headers from a request and return a string
async fn route_all(
    headers: HeaderMap,
    Extension(router): Extension<Arc<NodeRouter>>,
    body: String,
) -> impl IntoResponse {
    let j: serde_json::Value = match serde_json::from_str(&body) {
        Ok(j) => j,
        Err(e) => {
            tracing::error!("Couldn't deserialize request. Error: {}. Body: {}", e, body);
            return Response::builder()
                .status(StatusCode::BAD_REQUEST)
                .header(header::CONTENT_TYPE, "application/json")
                .body(make_error(&0, "Couldn't deserialize request body").to_string())
                .unwrap();
        }
    };

    let meth = match j["method"].as_str() {
        Some(meth) => meth,
        None => {
            tracing::error!("Request has no method field");
            return Response::builder()
                .status(StatusCode::BAD_REQUEST)
                .header(header::CONTENT_TYPE, "application/json")
                .body(make_error(&0, "Request has no method field").to_string())
                .unwrap();
        }
    };

    tracing::debug!("Request received, method: {}", j["method"]);

    if meth.starts_with("engine_") {
        tracing::trace!("Routing {} to engine route", j["method"]);

        let request: RpcRequest = match serde_json::from_str(&body) {
            Ok(request) => request,
            Err(e) => {
                tracing::error!("Error deserializing {} request: {}", j["method"], e);
                return Response::builder()
                    .status(StatusCode::BAD_REQUEST)
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(make_error(&0, "Error deserializing request").to_string())
                    .unwrap();
            }
        };

        let jwt_token = match headers.get("Authorization") {
            Some(jwt_token) => match jwt_token.to_str() {
                Ok(jwt_token) => jwt_token,
                Err(e) => {
                    tracing::error!("Error while converting jwt token to string: {}", e);
                    return Response::builder()
                        .status(StatusCode::BAD_REQUEST)
                        .header(header::CONTENT_TYPE, "application/json")
                        .body(
                            make_error(&0, "Error while converting jwt token to string")
                                .to_string(),
                        )
                        .unwrap();
                }
            },
            None => {
                tracing::error!("Request has no Authorization header");
                return Response::builder()
                    .status(StatusCode::BAD_REQUEST)
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(make_error(&0, "Request has no Authorization header").to_string())
                    .unwrap();
            }
        };

        let (resp, status) = router
            .do_engine_route(&router.fork_config, &request, jwt_token.to_string())
            .await;

        Response::builder()
            .status(status)
            .header(header::CONTENT_TYPE, "application/json")
            .body(resp)
            .unwrap()
    }
    // engine requests
    else {
        tracing::trace!("Routing to normal route");

        let jwt_token = headers.get("Authorization");
        if jwt_token.is_none() {
            let (resp, status) = router
                .do_route_normal(
                    body,
                    format!(
                        "Bearer {}",
                        make_jwt(&router.primary_node.read().await.jwt_key).unwrap()
                    ), // supporting requests without jwt tokens to authrpc is used for OE.
                ) // open an issue if you need this to be changed
                .await;

            return Response::builder()
                .status(status)
                .header(header::CONTENT_TYPE, "application/json")
                .body(resp)
                .unwrap();
        }

        let jwt_token = match headers.get("Authorization") {
            Some(header_value) => match header_value.to_str() {
                Ok(jwt_str) => jwt_str.to_string(),
                Err(e) => {
                    tracing::warn!(
                        "Could not extract authorization header from normal request: {}",
                        e
                    );
                    return Response::builder()
                        .status(400)
                        .header(header::CONTENT_TYPE, "application/json")
                        .body(r#"{"error": "Could not extract authorization header from normal request}"#.to_string())
                        .unwrap();
                }
            },
            None => {
                // should never happen, should've been caught before and been replaced
                return Response::builder()
                    .status(400)
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(
                        r#"{"error": "This should never happen, please open an issue.}"#
                            .to_string(),
                    )
                    .unwrap();
            }
        };

        let (resp, status) = router.do_route_normal(body, jwt_token).await;

        Response::builder()
            .status(status)
            .header(header::CONTENT_TYPE, "application/json")
            .body(resp)
            .unwrap()
    } // all other non-engine requests
}

async fn make_metrics_report(
    router: Arc<NodeRouter>,
) -> Result<serde_json::Value, serde_json::Error> {
    let syncing_nodes = router.alive_but_syncing_nodes.read().await;
    let alive_nodes = router.alive_nodes.read().await;
    let mut both = syncing_nodes.clone();
    both.append(&mut alive_nodes.clone());
    drop(alive_nodes);
    drop(syncing_nodes);

    let mut futs = Vec::new();
    both.iter().for_each(|node| {
        futs.push(async move { (node.url.clone(), node.status.read().await.resp_time) })
    });

    let resp_times: HashMap<String, u128> = join_all(futs).await.into_iter().collect();

    let metrics_report = MetricsReport {
        response_times: resp_times,
        alive_nodes: router
            .alive_nodes
            .read()
            .await
            .iter()
            .map(|node| node.url.clone())
            .collect(),
        syncing_nodes: router
            .alive_but_syncing_nodes
            .read()
            .await
            .iter()
            .map(|node| node.url.clone())
            .collect(),
        dead_nodes: router
            .dead_nodes
            .read()
            .await
            .iter()
            .map(|node| node.url.clone())
            .collect(),
        primary_node: router.primary_node.read().await.url.clone(),
    };

    serde_json::to_value(metrics_report)
}

async fn metrics(Extension(router): Extension<Arc<NodeRouter>>) -> impl IntoResponse {
    let report = match make_metrics_report(router).await {
        Ok(report) => report,
        Err(e) => {
            tracing::error!("Could not make metrics report: {}", e);
            json!({"error": format!("Could not make metrics report: {}", e)})
        }
    };

    let resp_body = match serde_json::to_string(&report) {
        Ok(resp_body) => resp_body,
        Err(e) => {
            tracing::error!("Unable to serialize metrics report: {}", e);
            r#"{"error":"Unable to serialize metrics report"}"#.to_string()
        }
    };

    Response::builder()
        .status(200)
        .header(header::CONTENT_TYPE, "application/json")
        .body(resp_body)
        .unwrap()
}

// calls router.recheck, returns recheck time, and metrics
async fn recheck(router: Arc<NodeRouter>) -> Result<(String, StatusCode), String> {
    let start = std::time::Instant::now();
    router.recheck().await;
    let resp_time = start.elapsed().as_micros();

    let mut report = match make_metrics_report(router).await {
        Ok(report) => report,
        Err(e) => {
            tracing::error!("Unable to get metrics report: {}", e);
            return Err(
                r#"{"error":"Unable to get metrics report; Recheck succeeded."}"#.to_string(),
            );
        }
    };

    report["recheck_time"] = serde_json::to_value(resp_time).unwrap();

    let resp_body = match serde_json::to_string(&report) {
        Ok(resp_body) => resp_body,
        Err(e) => {
            tracing::error!("Unable to serialize metrics report: {}", e);
            return Err(r#"{"error":"Unable to serialize metrics report"}"#.to_string());
        }
    };

    Ok((resp_body, StatusCode::OK))
}

async fn recheck_handler(Extension(router): Extension<Arc<NodeRouter>>) -> impl IntoResponse {
    match recheck(router).await {
        Ok((resp_body, status_code)) => Response::builder()
            .status(status_code)
            .header(header::CONTENT_TYPE, "application/json")
            .body(resp_body)
            .unwrap(),
        Err(e) => Response::builder()
            .status(500)
            .header(header::CONTENT_TYPE, "application/json")
            .body(e)
            .unwrap(),
    }
}

async fn add_node(
    Extension(router): Extension<Arc<NodeRouter>>,
    extract::Json(request): extract::Json<NodeList>,
) -> impl IntoResponse {
    let mut nodes = match request.create_new_nodes(router.general_jwt.clone()) {
        Ok(nodes) => nodes,
        Err(e) => {
            tracing::error!("Unable to create nodes from NodeList: {}", e);
            return Response::builder()
                .status(500)
                .header(header::CONTENT_TYPE, "application/json")
                .body(format!(
                    r#"{{"error":"Unable to get nodes from NodeList: {}"}}"#,
                    e
                ))
                .unwrap();
        }
    };

    tracing::info!("Adding {} new nodes", nodes.len());
    router.nodes.lock().await.append(&mut nodes);

    match recheck(router).await {
        Ok((resp_body, status_code)) => Response::builder()
            .status(status_code)
            .header(header::CONTENT_TYPE, "application/json")
            .body(resp_body)
            .unwrap(),
        Err(e) => Response::builder()
            .status(500)
            .header(header::CONTENT_TYPE, "application/json")
            .body(e)
            .unwrap(),
    }
}

#[tokio::main]
async fn main() {
    let matches = clap::App::new("executionbackup")
        .version(VERSION)
        .author("TennisBowling <tennisbowling@tennisbowling.com>")
        .setting(clap::AppSettings::ColoredHelp)
        .about("A Ethereum 2.0 multiplexer enabling execution node failover post-merge")
        .long_version(&*format!(
            "executionbackup version {} by TennisBowling <tennisbowling@tennisbowling.com>",
            VERSION
        ))
        .arg(
            clap::Arg::with_name("port")
                .short("p")
                .long("port")
                .value_name("PORT")
                .help("Port to listen on")
                .takes_value(true)
                .default_value("7000"),
        )
        .arg(
            clap::Arg::with_name("nodes")
                .short("n")
                .long("nodes")
                .value_name("NODES")
                .help("Comma-separated list of nodes to use")
                .takes_value(true)
                .required(true),
        )
        .arg(
            clap::Arg::with_name("jwt-secret")
                .short("j")
                .long("jwt-secret")
                .value_name("JWT")
                .help("Path to JWT secret file")
                .takes_value(true)
                .required(false),
        )
        .arg(
            clap::Arg::with_name("fcu-majority")
                .short("fcu")
                .long("fcu-majority")
                .value_name("FCU")
                .help("Threshold % (written like 0.1 for 10%) to call responses a majority from forkchoiceUpdated")
                .takes_value(true)
                .default_value("0.6"),
        )
        .arg(
            clap::Arg::with_name("listen-addr")
                .short("addr")
                .long("listen-addr")
                .value_name("LISTEN")
                .help("Address to listen on")
                .takes_value(true)
                .default_value("0.0.0.0"),
        )
        .arg(
            clap::Arg::with_name("log-level")
                .short("l")
                .long("log-level")
                .value_name("LOG")
                .help("Log level")
                .takes_value(true)
                .default_value("info"),
        )
        .arg(
            clap::Arg::with_name("node-timings")
            .long("node-timings")
            .help("Show node ping times")
        )
        .arg(
            clap::Arg::with_name("holesky")
                .long("holesky")
                .help("Enables configuration for the holesky testnet")
        )
        .get_matches();

    let port = matches.value_of("port").unwrap();
    let nodes = matches.value_of("nodes").unwrap();
    let jwt_secret_path = matches.value_of("jwt-secret");
    let fcu_majority = matches.value_of("fcu-majority").unwrap();
    let listen_addr = matches.value_of("listen-addr").unwrap();
    let log_level = matches.value_of("log-level").unwrap();
    let node_timings_enabled = matches.is_present("node-timings");
    let is_holesky = matches.is_present("holesky");

    // set log level with tracing subscriber
    let filter_string = format!("{},hyper=info", log_level);

    let filter = EnvFilter::try_new(filter_string).unwrap_or_else(|_| EnvFilter::new(log_level));

    let subscriber = tracing_subscriber::fmt::Subscriber::builder()
        .with_env_filter(filter)
        .finish();

    tracing::subscriber::set_global_default(subscriber).expect("Setting default subscriber failed");
    tracing::info!("Starting executionbackup version {VERSION}");

    tracing::info!("fcu invalid threshold set to: {}", fcu_majority);
    let fcu_majority = fcu_majority.parse::<f32>();
    let fcu_majority = match fcu_majority {
        Ok(fcu_majority) => {
            if !(0.0..=1.0).contains(&fcu_majority) {
                tracing::error!("fcu majority must be between 0.0 and 1.0");
                return;
            }
            fcu_majority
        }
        Err(e) => {
            tracing::error!("Error parsing fcu majority: {}", e);
            return;
        }
    };

    let nodes = nodes.split(',').collect::<Vec<&str>>();
    let mut nodesinstances: Vec<Arc<Node>> = Vec::new();

    let re = match Regex::new(r"#jwt-secret=(.*)") {
        Ok(re) => re,
        Err(e) => {
            tracing::error!("Failed to compile jwt matching secret: {}", e);
            return;
        }
    };

    let mut general_jwt: Option<jsonwebtoken::EncodingKey> = None;
    if let Some(general_jwt_path) = jwt_secret_path {
        general_jwt = Some(match read_jwt(general_jwt_path) {
            Ok(general_jwt) => general_jwt,
            Err(e) => {
                tracing::error!("Error reading encoding general jwt: {}", e);
                return;
            }
        });
    }

    for node in nodes.clone() {
        if let Some(captures) = re.captures(node) {
            if let Some(jwt_path) = captures.get(1) {
                let jwt_secret = match read_jwt(jwt_path.as_str()) {
                    Ok(jwt_secret) => jwt_secret,
                    Err(e) => {
                        tracing::error!("Could not encode jwt secret: {}", e);
                        return;
                    }
                };
                let node_str = re.replace(node, "").to_string();
                let node = Arc::new(Node::new(node_str, jwt_secret));
                nodesinstances.push(node);
                continue;
            }
        } else if let Some(general_jwt) = &general_jwt {
            nodesinstances.push(Arc::new(Node::new(node.to_string(), general_jwt.clone())))
        } else {
            tracing::error!("Node {} does not match specific or general jwt", node);
            return;
        }
    }

    let fork_config = match is_holesky {
        true => {
            tracing::info!("Running on holesky testnet");
            ForkConfig::holesky()
        }
        false => {
            tracing::info!("Running on mainnet");
            ForkConfig::mainnet()
        }
    };

    // guarenteed to have at least 1 node since clap enforces it
    let primary_node = nodesinstances.first().unwrap().clone();

    let router = Arc::new(NodeRouter::new(
        //jwt_secret,
        fcu_majority,
        nodesinstances,
        primary_node,
        node_timings_enabled,
        fork_config,
        general_jwt,
    ));

    // setup backround task to check if nodes are alive
    let router_clone = router.clone();
    tracing::debug!("Starting background recheck task");
    tokio::spawn(async move {
        loop {
            router_clone.recheck().await;
            tokio::time::sleep(Duration::from_secs(15)).await;
        }
    });

    // setup axum server
    let app = Router::new()
        .route("/", axum::routing::post(route_all))
        .route("/metrics", axum::routing::get(metrics))
        .route("/recheck", axum::routing::get(recheck_handler))
        .route("/add_nodes", axum::routing::post(add_node))
        .layer(Extension(router.clone()))
        .layer(DefaultBodyLimit::disable()); // no body limit since some requests can be quite large

    let addr = format!("{}:{}", listen_addr, port);
    let addr: SocketAddr = addr.parse().unwrap();
    let listener = match tokio::net::TcpListener::bind(addr).await {
        Ok(listener) => listener,
        Err(e) => {
            tracing::error!("Unable to bind to {}: {}", addr, e);
            return;
        }
    };
    tracing::info!("Listening on {}", addr);
    axum::serve(listener, app).await.unwrap();
}
