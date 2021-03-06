use byteorder::{LittleEndian, ReadBytesExt};

use crypto::digest::Digest;
use crypto::mac::Mac;
use crypto::hmac::Hmac;
use crypto::sha1::Sha1;
use crypto::sha2::Sha256;
use crypto::{symmetriccipher, buffer, aes, blockmodes};
use crypto::buffer::{ReadBuffer, WriteBuffer, BufferResult};

use futures_util::{future, StreamExt, SinkExt};
use futures_util::stream::{self, SplitSink};

use http::Request;

use rand::RngCore;
use rand::rngs::OsRng;

use rsa::{PublicKey, RSAPublicKey};

use std::collections::HashMap;
use std::convert::{TryFrom, TryInto};
use std::io::{self, Cursor, Read, Seek, SeekFrom};

use thiserror::Error;

use tokio::{net::TcpStream, stream::Stream, sync::mpsc};
use tokio_tungstenite::{connect_async, tungstenite, WebSocketStream};

use crate::loxapp3::{LoxoneMutation, LoxoneUUID, LoxoneState, LoxoneDaytimerEntry, LoxoneWeatherEntry};

/// WebSocket client for communicating with the Miniserver.
pub struct WebSocket {
    session: Option<Session>,
    rx: mpsc::UnboundedReceiver<Message>,
    sink: SplitSink<WebSocketStream<TcpStream>, tungstenite::Message>,
}

struct Session {
    rsa_key: [u8; 32],
    rsa_iv: [u8; 16],
    salt: [u8; 2],
    session_key: Vec<u8>,
}

/// Unbounded receiver for receiving state update events.
pub struct EventReceiver {
    rx: mpsc::UnboundedReceiver<EventTable>
}

enum MessageType {
    Text = 0,
    BinaryFile,
    ValueEventTable,
    TextEventTable,
    DaytimerEventTable,
    OutOfServiceIndicator,
    KeepAlive,
    WeatherEventTable,
}

#[derive(Debug)]
enum Message {
    Text(String),
    BinaryText(String),
    BinaryFile(Vec<u8>),
    EventTable(EventTable),
    OutOfServiceIndicator,
    KeepAlive,
}

#[derive(Debug)]
struct ValueEvent(LoxoneUUID, f64);
#[derive(Debug)]
struct TextEvent(LoxoneUUID, LoxoneUUID, String);
#[derive(Debug)]
struct DaytimerEvent(LoxoneUUID, f64, Vec<LoxoneDaytimerEntry>);
#[derive(Debug)]
struct WeatherEvent(LoxoneUUID, u32, Vec<LoxoneWeatherEntry>);

#[derive(Debug)]
enum EventTable {
    ValueEvents(Vec<ValueEvent>),
    TextEvents(Vec<TextEvent>),
    DaytimerEvents(Vec<DaytimerEvent>),
    WeatherEvents(Vec<WeatherEvent>),
}

#[derive(Error, Debug)]
pub enum X509CertError {
    #[error("pem error")]
    PemDecode(#[from] pem::PemError),
    #[error("asn1 error")]
    ASN1Decode(#[from] simple_asn1::ASN1DecodeErr),
    #[error("asn1 error")]
    ASN1MissingBlock,
    #[error("pkcs1 error")]
    PKCS1(#[from] rsa::errors::Error),
}

#[derive(Error, Debug)]
pub enum KeyExchangeError {
    #[error("invalid session key")]
    SessionKey(#[from] X509CertError),
    #[error("transport error")]
    Transport(#[from] tungstenite::Error),
    #[error("invalid reply message")]
    InvalidMessageType,
    #[error("invalid json reply")]
    JsonDeserialize(#[from] serde_json::Error),
    #[error("invalid json reply")]
    JsonMissingField(&'static str),
    #[error("invalid reply status code")]
    InvalidStatusCode(String),
    #[error("key decode key")]
    KeyDecode(#[from] base64::DecodeError),
}

#[derive(Error, Debug)]
pub enum RequestError {
    #[error("transport error")]
    Transport(#[from] tungstenite::Error),
    #[error("invalid reply type")]
    InvalidMessageType,
    #[error("invalid json reply")]
    JsonDeserialize(#[from] serde_json::Error),
    #[error("invalid json reply")]
    JsonMissingField(&'static str),
    #[error("invalid reply status code")]
    InvalidStatusCode(String),
}

#[derive(Error, Debug)]
pub enum AuthenticationError {
    #[error("transport error")]
    Transport(#[from] tungstenite::Error),
    #[error("invalid reply type")]
    InvalidMessageType,
    #[error("invalid json reply")]
    JsonDeserialize(#[from] serde_json::Error),
    #[error("invalid json reply")]
    JsonMissingField(&'static str),
    #[error("invalid reply status code")]
    InvalidStatusCode(String),
    #[error("key request errror")]
    KeyRequest(#[from] RequestError),
    #[error("key decode error")]
    KeyDecode(#[from] hex::FromHexError),
    #[error("invalid jwt token")]
    JwtBadFormat,
    #[error("invalid jwt token")]
    JwtDecode(#[from] base64::DecodeError),
}

#[derive(Error, Debug)]
pub enum JwtRequestError {
    #[error("transport error")]
    Transport(#[from] tungstenite::Error),
    #[error("invalid reply type")]
    InvalidMessageType,
    #[error("invalid json reply")]
    JsonDeserialize(#[from] serde_json::Error),
    #[error("invalid json reply")]
    JsonMissingField(&'static str),
    #[error("invalid reply status code")]
    InvalidStatusCode(String),
    #[error("key request errror")]
    KeyRequest(#[from] RequestError),
    #[error("key decode error")]
    KeyDecode(#[from] hex::FromHexError),
}

#[derive(Error, Debug)]
pub enum LoxAPP3RequestError {
    #[error("transport error")]
    Transport(#[from] tungstenite::Error),
    #[error("invalid reply type")]
    InvalidMessageType,
    #[error("invalid json reply")]
    JsonDeserialize(#[from] serde_json::Error),
}

impl WebSocket {
    /// Connects to the given WebSocket url.
    pub async fn connect(url: http::uri::Uri) -> Result<(Self, tungstenite::handshake::client::Response, EventReceiver, impl future::Future<Output = ()>), tungstenite::Error> {
        let request = Request::builder().uri(url).header("Sec-WebSocket-protocol", "remotecontrol").body(())?;
        let (ws_stream, resp) = connect_async(request).await?;
        let (sink, stream) = ws_stream.split();
        let (tx, rx) = mpsc::unbounded_channel();
        let (tx_events, rx_events) = mpsc::unbounded_channel();
        Ok((Self{sink, rx, session: None}, resp, EventReceiver::new(rx_events), Self::recv_loop(tx, tx_events, stream)))
    }

    /// Exchanges session key.
    pub async fn key_exchange(&mut self, cert: &str) -> Result<Vec<u8>, KeyExchangeError> {
        let session = Session::new(cert)?;
        match self.send_recv(&format!("jdev/sys/keyexchange/{}", base64::encode_config(&session, base64::STANDARD_NO_PAD))).await? {
            Message::Text(reply) => {
                let reply_json: serde_json::Map<String, serde_json::Value> = serde_json::from_str(&reply)?;
                match reply_json["LL"]["Code"].as_str() {
                    Some("200") => {
                        let remote_key = base64::decode(reply_json["LL"]["value"].as_str().ok_or(KeyExchangeError::JsonMissingField("LL.value"))?)?;
                        self.session = Some(session);
                        Ok(remote_key)
                    },
                    Some(status_code) => Err(KeyExchangeError::InvalidStatusCode(status_code.to_owned())),
                    None => Err(KeyExchangeError::JsonMissingField("LL.Code"))
                }
            },
            _reply => Err(KeyExchangeError::InvalidMessageType)
        }
    }

    /// Authenticates with the given token.
    pub async fn authenticate(&mut self, token: &str) -> Result<serde_json::Map<String, serde_json::Value>, AuthenticationError> {
        let key = &self.get_key().await?;
        let hash = hash_token(token, &hex::decode(&key)?, "SHA1");
        let payload: serde_json::Map<String, serde_json::Value> = serde_json::from_slice(&base64::decode(token.split('.').nth(1).ok_or(AuthenticationError::JwtBadFormat)?)?)?;
        match self.send_recv_enc(&format!("authwithtoken/{}/{}", hex::encode(hash), payload["user"].as_str().ok_or(RequestError::JsonMissingField("LL.value.user"))?)).await? {
            Message::Text(reply) => {
                let reply_json: serde_json::Map<String, serde_json::Value> = serde_json::from_str(&reply)?;
                match reply_json["LL"]["code"].as_str() {
                    Some("200") => Ok(reply_json["LL"]["value"].as_object().ok_or(AuthenticationError::JsonMissingField("LL.value"))?.to_owned()),
                    Some(status_code) => Err(AuthenticationError::InvalidStatusCode(status_code.to_owned())),
                    None => Err(AuthenticationError::JsonMissingField("LL.code"))
                }
            },
            _reply => Err(AuthenticationError::InvalidMessageType)
        }
    }

    async fn get_key(&mut self) -> Result<String, RequestError> {
        match self.send_recv("jdev/sys/getkey").await? {
            Message::Text(reply) => {
                let reply_json: serde_json::Map<String, serde_json::Value> = serde_json::from_str(&reply)?;
                match reply_json["LL"]["Code"].as_str() {
                    Some("200") => Ok(reply_json["LL"]["value"].as_str().ok_or(RequestError::JsonMissingField("LL.value"))?.to_owned()),
                    Some(status_code) => Err(RequestError::InvalidStatusCode(status_code.to_owned())),
                    None => Err(RequestError::JsonMissingField("LL.Code"))
                }
            },
            _reply => Err(RequestError::InvalidMessageType)
        }
    }

    async fn get_key_salt(&mut self, user: &str) -> Result<serde_json::Map<String, serde_json::Value>, RequestError> {
        match self.send_recv(&format!("jdev/sys/getkey2/{}", user)).await? {
            Message::Text(reply) => {
                let reply_json: serde_json::Map<String, serde_json::Value> = serde_json::from_str(&reply)?;
                match reply_json["LL"]["code"].as_str() {
                    Some("200") => Ok(reply_json["LL"]["value"].as_object().ok_or(RequestError::JsonMissingField("LL.value"))?.to_owned()),
                    Some(status_code) => Err(RequestError::InvalidStatusCode(status_code.to_owned())),
                    None => Err(RequestError::JsonMissingField("LL.code"))
                }
            },
            _reply => Err(RequestError::InvalidMessageType)
        }
    }

    /// Returns the JSON Web Token for the given authentication credentials.
    pub async fn get_jwt(&mut self, user: &str, password: &str, permission: u8, uuid: &str, info: &str) -> Result<serde_json::Map<String, serde_json::Value>, JwtRequestError> {
        let auth = self.get_key_salt(user).await?;
        let hash = hash_pwd(
            user,
            password,
            &hex::decode(auth["key"].as_str().ok_or(RequestError::JsonMissingField("LL.value.key"))?)?,
            auth["salt"].as_str().ok_or(RequestError::JsonMissingField("LL.value.salt"))?,
            auth["hashAlg"].as_str().ok_or(RequestError::JsonMissingField("LL.value.hashAlg"))?
        );

        match self.send_recv_enc(&format!("jdev/sys/getjwt/{}/{}/{}/{}/{}", hex::encode(hash), user, permission, uuid, info)).await? {
            Message::Text(reply) => {
                let reply_json: serde_json::Map<String, serde_json::Value> = serde_json::from_str(&reply.replace("\r", ""))?;
                match reply_json["LL"]["code"].as_str() {
                    Some("200") => Ok(reply_json["LL"]["value"].as_object().ok_or(JwtRequestError::JsonMissingField("LL.value"))?.to_owned()),
                    Some(status_code) => Err(JwtRequestError::InvalidStatusCode(status_code.to_owned())),
                    None => Err(JwtRequestError::JsonMissingField("LL.code"))
                }
            },
            _reply => Err(JwtRequestError::InvalidMessageType)
        }
    }

    /// Returns the LoxAPP3 structure file.
    pub async fn get_loxapp3<T: for<'de> serde::Deserialize<'de>>(&mut self) -> Result<T, LoxAPP3RequestError> {
        match self.send_recv("data/LoxAPP3.json").await? {
            Message::BinaryText(reply) => {
                let reply_json = serde_json::from_str(&reply)?;
                Ok(reply_json)
            },
            _reply => Err(LoxAPP3RequestError::InvalidMessageType)
        }
    }

    /// Returns the LoxAPP3.json update timestamp.
    pub async fn get_loxapp3_timestamp(&mut self) -> Result<String, RequestError> {
        match self.send_recv("jdev/sps/LoxAPPversion3").await? {
            Message::Text(reply) => {
                let reply_json: serde_json::Map<String, serde_json::Value> = serde_json::from_str(&reply)?;
                assert_eq!(reply_json["LL"]["Code"].as_str(), Some("200"));
                match reply_json["LL"]["Code"].as_str() {
                    Some("200") => Ok(reply_json["LL"]["value"].as_str().ok_or(RequestError::JsonMissingField("LL.value"))?.to_owned()),
                    Some(status_code) => Err(RequestError::InvalidStatusCode(status_code.to_owned())),
                    None => Err(RequestError::JsonMissingField("LL.Code"))
                }
            },
            _reply => Err(RequestError::InvalidMessageType)
        }
    }

    /// Enables status updates.
    pub async fn enable_status_update(&mut self, mut rx: EventReceiver) -> Result<(HashMap<LoxoneUUID, LoxoneState>, impl Stream<Item=(LoxoneUUID, LoxoneState)>), RequestError> {
        match self.send_recv("jdev/sps/enablebinstatusupdate").await? {
            Message::Text(reply) => {
                let reply_json: serde_json::Map<String, serde_json::Value> = serde_json::from_str(&reply)?;
                match reply_json["LL"]["Code"].as_str() {
                    Some("200") => {
                        assert_eq!(reply_json["LL"]["value"].as_str().ok_or(RequestError::JsonMissingField("LL.value"))?, "1");
                        let initial_state = rx.rx.by_ref().take(4).map(|event_table| event_table.into()).concat().await;
                        let stream = rx.rx.flat_map(|event_table|stream::iter::<HashMap<LoxoneUUID, LoxoneState>>(event_table.into()));
                        Ok((initial_state, stream))
                    },
                    Some(status_code) => Err(RequestError::InvalidStatusCode(status_code.to_owned())),
                    None => Err(RequestError::JsonMissingField("LL.Code"))
                }
            },
            _reply => Err(RequestError::InvalidMessageType)
        }
    }

    /// Sends the given `cmd` mutation to the given `control` UUID.
    pub async fn send_io_cmd(&mut self, control: &LoxoneUUID, cmd: LoxoneMutation) -> Result<(), RequestError> {
        match self.send_recv(&format!("jdev/sps/io/{}/{}", control, cmd)).await? {
            Message::Text(reply) => {
                let reply_json: serde_json::Map<String, serde_json::Value> = serde_json::from_str(&reply)?;
                match reply_json["LL"]["Code"].as_str() {
                    Some("200") => {
                        assert_eq!(reply_json["LL"]["value"].as_str().ok_or(RequestError::JsonMissingField("LL.value"))?, "1");
                        Ok(())
                    },
                    Some(status_code) => Err(RequestError::InvalidStatusCode(status_code.to_owned())),
                    None => Err(RequestError::JsonMissingField("LL.Code"))
                }
            },
            _reply => Err(RequestError::InvalidMessageType)
        }
    }

    async fn send_recv(&mut self, cmd: &str) -> Result<Message, tungstenite::Error> {
        self.sink.send(tungstenite::Message::from(cmd)).await?;
        self.recv().await
    }

    async fn send_recv_enc(&mut self, cmd: &str) -> Result<Message, tungstenite::Error> {
        let session = self.session.as_ref().ok_or(tungstenite::Error::from(io::Error::from(io::ErrorKind::PermissionDenied)))?;
        let encrypted_cmd = encrypt_cmd_ws("enc", &cmd, session).or(Err(tungstenite::Error::from(io::Error::new(io::ErrorKind::InvalidInput, cmd))))?;
        self.send_recv(&encrypted_cmd).await
    }

    async fn recv(&mut self) -> Result<Message, tungstenite::Error> {
        self.rx.recv().await.ok_or(tungstenite::Error::from(io::Error::from(io::ErrorKind::BrokenPipe)))
    }

    async fn recv_loop<S: StreamExt<Item=Result<tungstenite::Message, tungstenite::Error>> + Unpin>(tx: mpsc::UnboundedSender<Message>, tx_events: mpsc::UnboundedSender<EventTable>, stream: S) {
        let mut stream = stream.filter_map(|item| future::ready(item.ok()));
        while let Ok(msg) = parse_msg_next(&mut stream).await {
            match msg {
                Message::KeepAlive => println!("KEEP ALIVE"),
                Message::OutOfServiceIndicator => eprintln!("OUT OF SERVICE"),
                Message::EventTable(event_table) => tx_events.send(event_table).unwrap(),
                _ => tx.send(msg).unwrap()
            }
        }
    }
}

impl Session {
    fn new(cert: &str) -> Result<Self, X509CertError> {
        let public_key = parse_cert(cert)?;

        let mut rsa_key: [u8; 32] = [0; 32];
        OsRng.fill_bytes(&mut rsa_key);

        let mut rsa_iv: [u8; 16] = [0; 16];
        OsRng.fill_bytes(&mut rsa_iv);

        let mut salt: [u8; 2] = [0; 2];
        OsRng.fill_bytes(&mut salt);

        let mut session_key_rng = rand::rngs::OsRng;
        let session_key_data = format!("{}:{}", hex::encode(rsa_key), hex::encode(rsa_iv));
        let session_key = public_key.encrypt(&mut session_key_rng, rsa::PaddingScheme::PKCS1v15Encrypt, session_key_data.as_bytes())?;

        Ok(Self { session_key, rsa_key, rsa_iv, salt })
    }
}

impl AsRef<[u8]> for Session {
    fn as_ref(&self) -> &[u8] {
        &self.session_key
    }
}

impl EventReceiver {
    fn new(rx: mpsc::UnboundedReceiver<EventTable>) -> Self { Self{ rx } }
}

impl TryFrom<u8> for MessageType {
    type Error = io::Error;

    fn try_from(val: u8) -> Result<Self, Self::Error> {
        match val {
            0 => Ok(MessageType::Text),
            1 => Ok(MessageType::BinaryFile),
            2 => Ok(MessageType::ValueEventTable),
            3 => Ok(MessageType::TextEventTable),
            4 => Ok(MessageType::DaytimerEventTable),
            5 => Ok(MessageType::OutOfServiceIndicator),
            6 => Ok(MessageType::KeepAlive),
            7 => Ok(MessageType::WeatherEventTable),
            _ => Err(io::Error::from(io::ErrorKind::InvalidData)),
        }
    }
}

impl Into<HashMap<LoxoneUUID, LoxoneState>> for EventTable {
    fn into(self) -> HashMap<LoxoneUUID, LoxoneState> {
        match self { // TODO
            Self::ValueEvents(events) => events.into_iter().map(|event| (event.0, LoxoneState::Value(event.1))).collect(),
            Self::TextEvents(events) => events.into_iter().map(|event| (event.0, LoxoneState::Text(event.2, event.1))).collect(),
            Self::DaytimerEvents(events) => events.into_iter().map(|event| (event.0, LoxoneState::Daytimer(event.2, event.1))).collect(),
            Self::WeatherEvents(events) => events.into_iter().map(|event| (event.0, LoxoneState::Weather(event.2, event.1))).collect(),
        }
    }
}

fn hash_pwd(user: &str, pwd: &str, key: &[u8], salt: &str, hash_alg: &str) -> Vec<u8> {
    match hash_alg {
        "SHA1" => {
            let mut hasher = Sha1::new();
            hasher.input_str(format!("{}:{}", pwd, salt).as_str());
            let password_hash = hasher.result_str().to_uppercase();

            let mut mac = Hmac::<Sha1>::new(Sha1::new(), key);
            mac.input(format!("{}:{}", user, password_hash).as_bytes());

            let mac_result = mac.result();
            mac_result.code().to_vec()
        }
        "SHA256" => {
            let mut hasher = Sha256::new();
            hasher.input_str(format!("{}:{}", pwd, salt).as_str());
            let password_hash = hasher.result_str().to_uppercase();

            let mut mac = Hmac::<Sha256>::new(Sha256::new(), key);
            mac.input(format!("{}:{}", user, password_hash).as_bytes());

            let mac_result = mac.result();
            mac_result.code().to_vec()
        },
        _ => panic!("Can only use SHA1 and SHA256 here.")
    }
}

fn hash_token(token: &str, key: &[u8], hash_alg: &str) -> Vec<u8> {
    match hash_alg {
        "SHA1" => {
            let mut mac = Hmac::<Sha1>::new(Sha1::new(), key);
            mac.input(token.as_bytes());

            let mac_result = mac.result();
            mac_result.code().to_vec()
        }
        "SHA256" => {
            let mut mac = Hmac::<Sha256>::new(Sha256::new(), key);
            mac.input(token.as_bytes());

            let mac_result = mac.result();
            mac_result.code().to_vec()
        },
        _ => panic!("Can only use SHA1 and SHA256 here.")
    }
}

fn encrypt_cmd(cmd: &str, session: &Session) -> Result<Vec<u8>, symmetriccipher::SymmetricCipherError> {
    let salted_cmd = format!("salt/{}/{}\0", hex::encode(session.salt), cmd);

    let mut encryptor = aes::cbc_encryptor(aes::KeySize::KeySize256, &session.rsa_key, &session.rsa_iv, blockmodes::PkcsPadding);
    let mut final_result = Vec::<u8>::new();
    let mut read_buffer = buffer::RefReadBuffer::new(salted_cmd.as_bytes());
    let mut buffer = [0; 4096];
    let mut write_buffer = buffer::RefWriteBuffer::new(&mut buffer);

    loop {
        let result = encryptor.encrypt(&mut read_buffer, &mut write_buffer, true)?;
        final_result.extend(write_buffer.take_read_buffer().take_remaining().iter().map(|&i| i));

        match result {
            BufferResult::BufferUnderflow => break,
            BufferResult::BufferOverflow => { }
        }
    }

    Ok(final_result)
}

fn encrypt_cmd_ws(endpoint: &str, cmd: &str, session: &Session) -> Result<String, symmetriccipher::SymmetricCipherError> {
    let encoded_cipher: String = url::form_urlencoded::byte_serialize(base64::encode_config(encrypt_cmd(cmd, session)?, base64::STANDARD_NO_PAD).as_bytes()).collect();
    Ok(format!("jdev/sys/{}/{}", endpoint, encoded_cipher))
}

fn parse_cert(cert: &str) -> Result<RSAPublicKey, X509CertError> {
    let pem = pem::parse(cert)?;
    let asn1_blocks = simple_asn1::from_der(&pem.contents)?;

    match asn1_blocks.first() {
        Some(simple_asn1::ASN1Block::Sequence(_ofs, seq_blocks)) =>
            match seq_blocks.last() {
                Some(simple_asn1::ASN1Block::BitString(_ofs, _len, der)) => rsa::RSAPublicKey::from_pkcs1(der).map_err(|err| X509CertError::PKCS1(err)),
                _ => Err(X509CertError::ASN1MissingBlock)
            },
        _ => Err(X509CertError::ASN1MissingBlock)
    }
}

async fn parse_msg_next<S: StreamExt<Item=tungstenite::Message> + Unpin>(stream: &mut S) -> Result<Message, tungstenite::Error> {
    match stream.next().await.unwrap() {
        tungstenite::Message::Binary(msg) => {
            match parse_msg_header(&msg) {
                (msg_type, Some(msg_len)) =>
                    Ok(parse_msg_body(msg_type, msg_len.try_into().unwrap(), stream).await),
                (msg_type, None) =>
                    Ok(parse_msg_body(msg_type, parse_msg_len(stream.next().await.unwrap()), stream).await)
            }
        },
        msg => panic!("invalid message header {:?}", msg)
    }
}

fn parse_msg_header(mut header: &[u8]) -> (MessageType, Option<usize>) {
    assert_eq!(header[0], header.read_u8().unwrap());
    let msg_type = MessageType::try_from(header.read_u8().unwrap()).unwrap();
    let msg_info = header.read_u8().unwrap();
    header.read_u8().unwrap();
    match msg_info {
        0 => (msg_type, Some(header.read_u32::<LittleEndian>().unwrap().try_into().unwrap())),
        _ => (msg_type, None)
    }
}

fn parse_msg_len(header_msg: tungstenite::Message) -> u64 {
    let mut header = Cursor::new(header_msg.into_data());
    header.read_u32::<LittleEndian>().unwrap().try_into().unwrap()
}

async fn parse_msg_body<S: StreamExt<Item=tungstenite::Message> + Unpin>(msg_type: MessageType, msg_len: u64, stream: &mut S) -> Message {
    match msg_type {
        MessageType::Text => {
            match stream.next().await.unwrap() {
                tungstenite::Message::Text(body_msg) => Message::Text(body_msg),
                msg => panic!("invalid message body {:?}", msg)
            }
        },
        MessageType::BinaryFile => {
            match stream.next().await.unwrap() {
                tungstenite::Message::Text(body_msg) => Message::BinaryText(body_msg),
                tungstenite::Message::Binary(body_msg) => Message::BinaryFile(body_msg),
                msg => panic!("invalid message body {:?}", msg)
            }
        },
        MessageType::ValueEventTable => {
            match stream.next().await.unwrap() {
                tungstenite::Message::Binary(body_msg) => {
                    let mut pack = Cursor::new(body_msg);
                    let mut events: Vec<ValueEvent> = Vec::new();
                    while pack.position() < msg_len {
                        let uuid = parse_uuid(&mut pack);
                        let val = pack.read_f64::<LittleEndian>().unwrap();
                        events.push(ValueEvent(uuid, val));
                    }
                    Message::EventTable(EventTable::ValueEvents(events))
                },
                msg => panic!("invalid message body {:?}", msg)
            }
        },
        MessageType::TextEventTable => {
            match stream.next().await.unwrap() {
                tungstenite::Message::Binary(body_msg) => {
                    let mut pack = Cursor::new(body_msg);
                    let mut events: Vec<TextEvent> = Vec::new();
                    while pack.position() < msg_len {
                        let uuid = parse_uuid(&mut pack);
                        let uuid_icon = parse_uuid(&mut pack);
                        let text_len = pack.read_u32::<LittleEndian>().unwrap().try_into().unwrap();
                        let mut text_buf = vec![0; text_len];
                        pack.read_exact(&mut text_buf).unwrap();
                        let text = String::from_utf8(text_buf).unwrap();
                        events.push(TextEvent(uuid, uuid_icon, text));
                        match text_len % 4 {
                            0 => (),
                            r => {
                                pack.seek(SeekFrom::Current((4 - r).try_into().unwrap())).unwrap();
                            }
                        }
                    }
                    Message::EventTable(EventTable::TextEvents(events))
                },
                msg => panic!("invalid message body {:?}", msg)
            }
        }
        MessageType::DaytimerEventTable => {
            match stream.next().await.unwrap() {
                tungstenite::Message::Binary(body_msg) => {
                    let mut pack = Cursor::new(body_msg);
                    let mut events: Vec<DaytimerEvent> = Vec::new();
                    while pack.position() < msg_len {
                        let uuid = parse_uuid(&mut pack);
                        let default_val = pack.read_f64::<LittleEndian>().unwrap();
                        let entries_len: usize = pack.read_i32::<LittleEndian>().unwrap().try_into().unwrap();
                        let mut entries: Vec<LoxoneDaytimerEntry> = Vec::new();
                        for _ in 0..entries_len {
                            let mode = pack.read_i32::<LittleEndian>().unwrap();
                            let from = pack.read_i32::<LittleEndian>().unwrap();
                            let to = pack.read_i32::<LittleEndian>().unwrap();
                            let need_activate = pack.read_i32::<LittleEndian>().unwrap();
                            let value = pack.read_f64::<LittleEndian>().unwrap();
                            entries.push(LoxoneDaytimerEntry{ mode, from, to, need_activate, value })
                        }
                        events.push(DaytimerEvent(uuid, default_val, entries))
                    }
                    Message::EventTable(EventTable::DaytimerEvents(events))
                },
                msg => panic!("invalid message body {:?}", msg)
            }
        },
        MessageType::OutOfServiceIndicator => Message::OutOfServiceIndicator,
        MessageType::KeepAlive => Message::KeepAlive,
        MessageType::WeatherEventTable => {
            match stream.next().await.unwrap() {
                tungstenite::Message::Binary(body_msg) => {
                    let mut pack = Cursor::new(body_msg);
                    let mut events: Vec<WeatherEvent> = Vec::new();
                    while pack.position() < msg_len {
                        let uuid = parse_uuid(&mut pack);
                        let last_update = pack.read_u32::<LittleEndian>().unwrap();
                        let entries_len: usize = pack.read_i32::<LittleEndian>().unwrap().try_into().unwrap();
                        let mut entries: Vec<LoxoneWeatherEntry> = Vec::new();
                        for _ in 0..entries_len {
                            let timestamp = pack.read_i32::<LittleEndian>().unwrap();
                            let weather_type = pack.read_i32::<LittleEndian>().unwrap();
                            let wind_direction = pack.read_i32::<LittleEndian>().unwrap();
                            let solar_radiation = pack.read_i32::<LittleEndian>().unwrap();
                            let relative_humidity = pack.read_i32::<LittleEndian>().unwrap();
                            let temperature = pack.read_f64::<LittleEndian>().unwrap();
                            let perceived_temperature = pack.read_f64::<LittleEndian>().unwrap();
                            let dew_point = pack.read_f64::<LittleEndian>().unwrap();
                            let precipitation = pack.read_f64::<LittleEndian>().unwrap();
                            let wind_speed = pack.read_f64::<LittleEndian>().unwrap();
                            let barometic_pressure = pack.read_f64::<LittleEndian>().unwrap();
                            entries.push(LoxoneWeatherEntry{
                                timestamp,
                                weather_type,
                                wind_direction,
                                solar_radiation,
                                relative_humidity,
                                temperature,
                                perceived_temperature,
                                dew_point,
                                precipitation,
                                wind_speed,
                                barometic_pressure
                            })
                        }
                        events.push(WeatherEvent(uuid, last_update, entries))
                    }
                    Message::EventTable(EventTable::WeatherEvents(events))
                },
                msg => panic!("invalid message body {:?}", msg)
            }
        },
    }
}

fn parse_uuid(pack: &mut Cursor<Vec<u8>>) -> LoxoneUUID {
    let d1 = pack.read_u32::<LittleEndian>().unwrap();
    let d2 = pack.read_u16::<LittleEndian>().unwrap();
    let d3 = pack.read_u16::<LittleEndian>().unwrap();
    let mut d4 = [0; 8];
    pack.read_exact(&mut d4).unwrap();
    format!("{:08x}-{:04x}-{:04x}-{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}", d1, d2, d3, d4[0], d4[1], d4[2], d4[3], d4[4], d4[5], d4[6], d4[7])
}
