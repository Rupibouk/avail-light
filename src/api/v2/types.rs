use anyhow::Context;
use avail_subxt::api::runtime_types::{
	avail_core::{data_lookup::compact::CompactDataLookup, header::extension::HeaderExtension},
	bounded_collections::bounded_vec::BoundedVec,
};
use base64::{engine::general_purpose, DecodeError, Engine};
use codec::Encode;
use derive_more::From;
use hyper::{http, StatusCode};
use kate_recovery::{commitments, config, matrix::Partition};
use serde::{de, Deserialize, Deserializer, Serialize, Serializer};
use sp_core::{blake2_256, H256};
use std::{
	collections::{HashMap, HashSet},
	sync::Arc,
};
use tokio::sync::{mpsc::UnboundedSender, RwLock};
use uuid::Uuid;
use warp::{
	ws::{self, Message},
	Reply,
};

use crate::{
	rpc::Node,
	types::{self, block_matrix_partition_format, RuntimeConfig, State},
};

#[derive(Debug)]
pub struct InternalServerError {}

impl warp::reject::Reject for InternalServerError {}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Version {
	pub version: String,
	pub network_version: String,
}

impl Reply for Version {
	fn into_response(self) -> warp::reply::Response {
		warp::reply::json(&self).into_response()
	}
}

#[derive(Serialize, Deserialize)]
pub struct BlockRange {
	pub first: u32,
	pub last: u32,
}

impl From<&types::BlockRange> for BlockRange {
	fn from(value: &types::BlockRange) -> Self {
		BlockRange {
			first: value.first,
			last: value.last,
		}
	}
}

#[derive(Serialize, Deserialize)]
pub struct HistoricalSync {
	pub synced: bool,
	#[serde(skip_serializing_if = "Option::is_none")]
	pub available: Option<BlockRange>,
	#[serde(skip_serializing_if = "Option::is_none")]
	pub app_data: Option<BlockRange>,
}

#[derive(Serialize, Deserialize)]
pub struct Blocks {
	pub latest: u32,
	#[serde(skip_serializing_if = "Option::is_none")]
	pub available: Option<BlockRange>,
	#[serde(skip_serializing_if = "Option::is_none")]
	pub app_data: Option<BlockRange>,
	#[serde(skip_serializing_if = "Option::is_none")]
	pub historical_sync: Option<HistoricalSync>,
}

#[derive(Serialize, Deserialize)]
pub struct Status {
	pub modes: Vec<Mode>,
	#[serde(skip_serializing_if = "Option::is_none")]
	pub app_id: Option<u32>,
	pub genesis_hash: String,
	pub network: String,
	pub blocks: Blocks,
	#[serde(
		skip_serializing_if = "Option::is_none",
		with = "block_matrix_partition_format"
	)]
	pub partition: Option<Partition>,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(try_from = "String")]
pub struct Base64(pub Vec<u8>);

impl From<Base64> for BoundedVec<u8> {
	fn from(val: Base64) -> Self {
		BoundedVec(val.0)
	}
}

impl From<Base64> for Vec<u8> {
	fn from(val: Base64) -> Self {
		val.0
	}
}

impl TryFrom<String> for Base64 {
	type Error = DecodeError;

	fn try_from(value: String) -> Result<Self, Self::Error> {
		general_purpose::STANDARD.decode(value).map(Base64)
	}
}

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Transaction {
	Data(Base64),
	Extrinsic(Base64),
}

impl Transaction {
	pub fn is_empty(&self) -> bool {
		match self {
			Transaction::Data(data) => data.0.is_empty(),
			Transaction::Extrinsic(data) => data.0.is_empty(),
		}
	}
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SubmitResponse {
	pub block_hash: H256,
	pub hash: H256,
	pub index: u32,
}

impl Reply for SubmitResponse {
	fn into_response(self) -> warp::reply::Response {
		warp::reply::json(&self).into_response()
	}
}

impl Status {
	pub fn new(config: &RuntimeConfig, node: &Node, state: &State) -> Self {
		let historical_sync = state.synced.map(|synced| HistoricalSync {
			synced,
			available: state.sync_confidence_achieved.as_ref().map(From::from),
			app_data: state.sync_data_verified.as_ref().map(From::from),
		});

		let blocks = Blocks {
			latest: state.latest,
			available: state.confidence_achieved.as_ref().map(From::from),
			app_data: state.data_verified.as_ref().map(From::from),
			historical_sync,
		};

		Status {
			modes: config.into(),
			app_id: config.app_id,
			genesis_hash: format!("{:?}", node.genesis_hash),
			network: node.network(),
			blocks,
			partition: config.block_matrix_partition,
		}
	}
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Mode {
	Light,
	App,
	Partition,
}

impl From<&RuntimeConfig> for Vec<Mode> {
	fn from(value: &RuntimeConfig) -> Self {
		let mut result: Vec<Mode> = vec![];
		result.push(Mode::Light);
		if value.app_id.is_some() {
			result.push(Mode::App);
		}
		if value.block_matrix_partition.is_some() {
			result.push(Mode::Partition)
		}
		result
	}
}

impl Reply for Status {
	fn into_response(self) -> warp::reply::Response {
		warp::reply::json(&self).into_response()
	}
}

#[derive(Serialize, Deserialize, PartialEq, Eq, Hash)]
#[serde(rename_all = "kebab-case")]
pub enum Topic {
	HeaderVerified,
	ConfidenceAchieved,
	DataVerified,
}

#[derive(Serialize, Deserialize, PartialEq, Eq, Hash)]
#[serde(rename_all = "kebab-case")]
pub enum DataField {
	Data,
	Extrinsic,
}

#[derive(Serialize, Deserialize, PartialEq, Default)]
pub struct Subscription {
	pub topics: HashSet<Topic>,
	pub data_fields: HashSet<DataField>,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct HeaderMessage {
	block_number: u32,
	header: Header,
}

impl TryFrom<avail_subxt::primitives::Header> for HeaderMessage {
	type Error = anyhow::Error;

	fn try_from(header: avail_subxt::primitives::Header) -> Result<Self, Self::Error> {
		let header: Header = header.try_into()?;
		Ok(Self {
			block_number: header.number,
			header,
		})
	}
}

#[derive(Debug, Serialize, Deserialize, Clone)]
struct Header {
	hash: H256,
	parent_hash: H256,
	pub number: u32,
	state_root: H256,
	extrinsics_root: H256,
	extension: Extension,
}

#[derive(Debug, Clone)]
struct Commitment([u8; config::COMMITMENT_SIZE]);

impl Serialize for Commitment {
	fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
	where
		S: Serializer,
	{
		let hex_string = format!("0x{}", hex::encode(self.0));
		serializer.serialize_str(&hex_string)
	}
}

impl<'de> Deserialize<'de> for Commitment {
	fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
	where
		D: Deserializer<'de>,
	{
		const LEN: usize = config::COMMITMENT_SIZE * 2 + 2;

		let s = String::deserialize(deserializer)?;

		if !s.starts_with("0x") || s.len() != LEN {
			let message = "Expected a hex string of correct length with 0x prefix";
			return Err(de::Error::custom(message));
		}

		let decoded = hex::decode(&s[2..]).map_err(de::Error::custom)?;
		let decoded_len = decoded.len();
		let bytes: [u8; config::COMMITMENT_SIZE] = decoded
			.try_into()
			.map_err(|_| de::Error::invalid_length(decoded_len, &"Expected vector of 48 bytes"))?;

		Ok(Commitment(bytes))
	}
}

#[derive(Debug, Serialize, Deserialize, Clone)]
struct Extension {
	rows: u16,
	cols: u16,
	data_root: Option<H256>,
	commitments: Vec<Commitment>,
	app_lookup: CompactDataLookup,
}

impl TryFrom<avail_subxt::primitives::Header> for Header {
	type Error = anyhow::Error;

	fn try_from(header: avail_subxt::primitives::Header) -> anyhow::Result<Self> {
		Ok(Header {
			hash: Encode::using_encoded(&header, blake2_256).into(),
			parent_hash: header.parent_hash,
			number: header.number,
			state_root: header.state_root,
			extrinsics_root: header.extrinsics_root,
			extension: header.extension.try_into()?,
		})
	}
}

impl TryFrom<HeaderExtension> for Extension {
	type Error = anyhow::Error;

	fn try_from(value: HeaderExtension) -> Result<Self, Self::Error> {
		match value {
			HeaderExtension::V1(v1) => {
				let commitments = commitments::from_slice(&v1.commitment.commitment)?
					.into_iter()
					.map(Commitment)
					.collect::<Vec<_>>();
				Ok(Extension {
					rows: v1.commitment.rows,
					cols: v1.commitment.cols,
					data_root: Some(v1.commitment.data_root),
					commitments,
					app_lookup: v1.app_lookup,
				})
			},

			HeaderExtension::V2(v2) => {
				let commitments = commitments::from_slice(&v2.commitment.commitment)?
					.into_iter()
					.map(Commitment)
					.collect::<Vec<_>>();

				Ok(Extension {
					rows: v2.commitment.rows,
					cols: v2.commitment.cols,
					data_root: v2.commitment.data_root,
					commitments,
					app_lookup: v2.app_lookup,
				})
			},
		}
	}
}

#[derive(Serialize, Clone)]
#[serde(tag = "type", content = "message", rename_all = "kebab-case")]
pub enum PublishMessage {
	HeaderVerified(HeaderMessage),
}

impl TryFrom<PublishMessage> for Message {
	type Error = anyhow::Error;
	fn try_from(value: PublishMessage) -> Result<Self, Self::Error> {
		serde_json::to_string(&value)
			.map(ws::Message::text)
			.context("Cannot serialize publish message")
	}
}

pub type Sender = UnboundedSender<Result<ws::Message, warp::Error>>;

pub struct WsClient {
	pub subscription: Subscription,
	pub sender: Option<Sender>,
}

impl WsClient {
	pub fn new(subscription: Subscription) -> Self {
		WsClient {
			subscription,
			sender: None,
		}
	}

	fn is_subscribed(&self, topic: &Topic) -> bool {
		self.subscription.topics.contains(topic)
	}
}

#[derive(Clone)]
pub struct WsClients(pub Arc<RwLock<HashMap<String, WsClient>>>);

impl WsClients {
	pub async fn set_sender(&self, subscription_id: &str, sender: Sender) -> bool {
		let mut clients = self.0.write().await;
		let Some(client) = clients.get_mut(subscription_id) else {
			return false;
		};
		client.sender = Some(sender);
		true
	}

	pub async fn has_subscription(&self, subscription_id: &str) -> bool {
		self.0.read().await.contains_key(subscription_id)
	}

	pub async fn subscribe(&self, subscription_id: String, subscription: Subscription) {
		let mut clients = self.0.write().await;
		clients.insert(subscription_id.clone(), WsClient::new(subscription));
	}

	pub async fn publish(&self, topic: Topic, message: PublishMessage) -> anyhow::Result<()> {
		let clients = self.0.read().await;
		for (_, client) in clients.iter() {
			if !client.is_subscribed(&topic) {
				continue;
			}
			let message = message.clone().try_into()?;
			if let Some(sender) = &client.sender {
				let _ = sender.send(Ok(message));
				// TODO: Aggregate errors
			}
		}
		Ok(())
	}
}

impl Default for WsClients {
	fn default() -> Self {
		Self(Arc::new(RwLock::new(HashMap::new())))
	}
}

#[derive(Serialize, Deserialize)]
pub struct SubscriptionId {
	pub subscription_id: String,
}

impl Reply for SubscriptionId {
	fn into_response(self) -> warp::reply::Response {
		warp::reply::json(&self).into_response()
	}
}

#[derive(Deserialize)]
#[serde(tag = "type", content = "message", rename_all = "kebab-case")]
pub enum Payload {
	Version,
	Status,
	Submit(Transaction),
}

#[derive(Deserialize)]
pub struct Request {
	#[serde(flatten)]
	pub payload: Payload,
	pub request_id: Uuid,
}

#[derive(Serialize, Deserialize)]
pub struct Response<T> {
	pub request_id: Uuid,
	pub message: T,
}

impl<T> Response<T> {
	pub fn new(request_id: Uuid, message: T) -> Self {
		Response {
			request_id,
			message,
		}
	}
}

impl TryFrom<ws::Message> for Request {
	type Error = anyhow::Error;

	fn try_from(value: ws::Message) -> Result<Self, Self::Error> {
		serde_json::from_slice(value.as_bytes()).context("Cannot parse json")
	}
}

#[derive(Serialize, Deserialize, PartialEq, Debug)]
#[serde(rename_all = "kebab-case")]
pub enum ErrorCode {
	NotFound,
	BadRequest,
	InternalServerError,
}

#[derive(Serialize, Deserialize)]
pub struct Error {
	#[serde(skip_serializing_if = "Option::is_none")]
	pub request_id: Option<Uuid>,
	#[serde(skip)]
	pub cause: Option<anyhow::Error>,
	pub error_code: ErrorCode,
	pub message: String,
}

impl Error {
	fn new(
		request_id: Option<Uuid>,
		cause: Option<anyhow::Error>,
		error_code: ErrorCode,
		message: &str,
	) -> Self {
		Error {
			request_id,
			cause,
			error_code,
			message: message.to_string(),
		}
	}

	pub fn not_found() -> Self {
		Self::new(None, None, ErrorCode::NotFound, "Not Found")
	}

	pub fn internal_server_error(cause: anyhow::Error) -> Self {
		Self::new(
			None,
			Some(cause),
			ErrorCode::InternalServerError,
			"Internal Server Error",
		)
	}

	pub fn bad_request_unknown(message: &str) -> Self {
		Self::new(None, None, ErrorCode::BadRequest, message)
	}

	pub fn bad_request(request_id: Uuid, message: &str) -> Self {
		Self::new(Some(request_id), None, ErrorCode::BadRequest, message)
	}

	fn status(&self) -> StatusCode {
		match self.error_code {
			ErrorCode::NotFound => StatusCode::NOT_FOUND,
			ErrorCode::BadRequest => StatusCode::BAD_REQUEST,
			ErrorCode::InternalServerError => StatusCode::INTERNAL_SERVER_ERROR,
		}
	}
}

impl Reply for Error {
	fn into_response(self) -> warp::reply::Response {
		http::Response::builder()
			.status(self.status())
			.body(self.message.clone())
			.expect("Can create error response")
			.into_response()
	}
}

impl From<Error> for String {
	fn from(error: Error) -> Self {
		serde_json::to_string(&error).expect("Error is serializable")
	}
}

pub fn handle_result(result: Result<impl Reply, impl Reply>) -> impl Reply {
	match result {
		Ok(ok) => ok.into_response(),
		Err(err) => err.into_response(),
	}
}

#[derive(Serialize, Deserialize, From)]
#[serde(tag = "topic", rename_all = "kebab-case")]
pub enum WsResponse {
	Version(Response<Version>),
	Status(Response<Status>),
	DataTransactionSubmitted(Response<SubmitResponse>),
}

#[derive(Serialize, Deserialize, From)]
#[serde(tag = "topic", rename_all = "kebab-case")]
pub enum WsError {
	Error(Error),
}
