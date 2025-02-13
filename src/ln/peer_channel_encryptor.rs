use ln::msgs;
use ln::msgs::HandleError;

use bitcoin_hashes::sha256::Hash as Sha256;
use bitcoin_hashes::{Hash, HashEngine, Hmac, HmacEngine};

use secp256k1;
use secp256k1::ecdh::SharedSecret;
use secp256k1::key::{PublicKey, SecretKey};
use secp256k1::Secp256k1;

use std::marker::PhantomData;

use util::byte_utils;
use util::chacha20poly1305rfc::ChaCha20Poly1305RFC;

// Sha256("Noise_XK_secp256k1_ChaChaPoly_SHA256")
const NOISE_CK: [u8; 32] = [
	0x26, 0x40, 0xf5, 0x2e, 0xeb, 0xcd, 0x9e, 0x88, 0x29, 0x58, 0x95, 0x1c, 0x79, 0x42, 0x50, 0xee,
	0xdb, 0x28, 0x00, 0x2c, 0x05, 0xd7, 0xdc, 0x2e, 0xa0, 0xf1, 0x95, 0x40, 0x60, 0x42, 0xca, 0xf1,
];
// Sha256(NOISE_CK || "lightning")
const NOISE_H: [u8; 32] = [
	0xd1, 0xfb, 0xf6, 0xde, 0xe4, 0xf6, 0x86, 0xf1, 0x32, 0xfd, 0x70, 0x2c, 0x4a, 0xbf, 0x8f, 0xba,
	0x4b, 0xb4, 0x20, 0xd8, 0x9d, 0x2a, 0x04, 0x8a, 0x3c, 0x4f, 0x4c, 0x09, 0x2e, 0x37, 0xb6, 0x76,
];

pub trait Direction {}
pub struct Inbound;
impl Direction for Inbound {}
pub struct Outbound;
impl Direction for Outbound {}

pub struct OutboundData {
	ie: SecretKey,
	their_node_id: PublicKey,
}

pub trait NoiseStep {
	type DirectionalNoiseState;
}
pub struct PreActOne<T: Direction>(pub PhantomData<T>);
pub struct InboundPreActOne;
impl NoiseStep for PreActOne<Inbound> {
	type DirectionalNoiseState = InboundPreActOne;
}
impl NoiseStep for PreActOne<Outbound> {
	type DirectionalNoiseState = OutboundData;
}
pub struct PostActOne<T: Direction>(pub PhantomData<T>);
pub struct InboundPostActOne {
	ie: PublicKey,
}
impl NoiseStep for PostActOne<Inbound> {
	type DirectionalNoiseState = InboundPostActOne;
}
impl NoiseStep for PostActOne<Outbound> {
	type DirectionalNoiseState = OutboundData;
}
pub struct PostActTwo<T: Direction>(pub PhantomData<T>);
pub struct InboundPostActTwo {
	ie: PublicKey,
	re: SecretKey,
	temp_k2: [u8; 32],
}
impl NoiseStep for PostActTwo<Inbound> {
	type DirectionalNoiseState = InboundPostActTwo;
}
impl NoiseStep for PostActTwo<Outbound> {
	type DirectionalNoiseState = OutboundData;
}

pub trait NoiseState {}
pub struct InProgress<T: NoiseStep> {
	state: PhantomData<T>,
	directional_state: T::DirectionalNoiseState,
	bidirectional_state: BidirectionalNoiseState,
}
impl<T> NoiseState for InProgress<T> where T: NoiseStep {}
pub struct Finished {
	sk: [u8; 32],
	sn: u64,
	sck: [u8; 32],
	rk: [u8; 32],
	rn: u64,
	rck: [u8; 32],
}
impl NoiseState for Finished {}

pub struct BidirectionalNoiseState {
	h: [u8; 32],
	ck: [u8; 32],
}

pub struct PeerChannelEncryptor<T: NoiseState> {
	secp_ctx: Secp256k1<secp256k1::SignOnly>,
	noise_state: T,
}

impl PeerChannelEncryptor<InProgress<PreActOne<Outbound>>> {
	pub fn new_outbound(their_node_id: PublicKey, ephemeral_key: SecretKey) -> Self {
		let secp_ctx = Secp256k1::signing_only();

		let mut sha = Sha256::engine();
		sha.input(&NOISE_H);
		sha.input(&their_node_id.serialize()[..]);
		let h = Sha256::from_engine(sha).into_inner();

		PeerChannelEncryptor {
			secp_ctx: secp_ctx,
			noise_state: InProgress {
				state: PhantomData,
				directional_state: OutboundData {
					ie: ephemeral_key,
					their_node_id,
				},
				bidirectional_state: BidirectionalNoiseState { h: h, ck: NOISE_CK },
			},
		}
	}
}

impl PeerChannelEncryptor<InProgress<PreActOne<Inbound>>> {
	pub fn new_inbound(our_node_secret: &SecretKey) -> Self {
		let secp_ctx = Secp256k1::signing_only();

		let mut sha = Sha256::engine();
		sha.input(&NOISE_H);
		let our_node_id = PublicKey::from_secret_key(&secp_ctx, our_node_secret);
		sha.input(&our_node_id.serialize()[..]);
		let h = Sha256::from_engine(sha).into_inner();

		PeerChannelEncryptor {
			secp_ctx: secp_ctx,
			noise_state: InProgress {
				state: PhantomData,
				directional_state: InboundPreActOne,
				bidirectional_state: BidirectionalNoiseState { h: h, ck: NOISE_CK },
			},
		}
	}
}

impl<T> PeerChannelEncryptor<T>
where
	T: NoiseState,
{
	#[inline]
	fn encrypt_with_ad(res: &mut [u8], n: u64, key: &[u8; 32], h: &[u8], plaintext: &[u8]) {
		let mut nonce = [0; 12];
		nonce[4..].copy_from_slice(&byte_utils::le64_to_array(n));

		let mut chacha = ChaCha20Poly1305RFC::new(key, &nonce, h);
		let mut tag = [0; 16];
		chacha.encrypt(plaintext, &mut res[0..plaintext.len()], &mut tag);
		res[plaintext.len()..].copy_from_slice(&tag);
	}

	#[inline]
	fn decrypt_with_ad(
		res: &mut [u8],
		n: u64,
		key: &[u8; 32],
		h: &[u8],
		cyphertext: &[u8],
	) -> Result<(), HandleError> {
		let mut nonce = [0; 12];
		nonce[4..].copy_from_slice(&byte_utils::le64_to_array(n));

		let mut chacha = ChaCha20Poly1305RFC::new(key, &nonce, h);
		if !chacha.decrypt(
			&cyphertext[0..cyphertext.len() - 16],
			res,
			&cyphertext[cyphertext.len() - 16..],
		) {
			return Err(HandleError {
				err: "Bad MAC",
				action: Some(msgs::ErrorAction::DisconnectPeer { msg: None }),
			});
		}
		Ok(())
	}

	fn hkdf_extract_expand(salt: &[u8], ikm: &[u8]) -> ([u8; 32], [u8; 32]) {
		let mut hmac = HmacEngine::<Sha256>::new(salt);
		hmac.input(ikm);
		let prk = Hmac::from_engine(hmac).into_inner();
		let mut hmac = HmacEngine::<Sha256>::new(&prk[..]);
		hmac.input(&[1; 1]);
		let t1 = Hmac::from_engine(hmac).into_inner();
		let mut hmac = HmacEngine::<Sha256>::new(&prk[..]);
		hmac.input(&t1);
		hmac.input(&[2; 1]);
		(t1, Hmac::from_engine(hmac).into_inner())
	}

	#[inline]
	fn hkdf(state: &mut BidirectionalNoiseState, ss: SharedSecret) -> [u8; 32] {
		let (t1, t2) = Self::hkdf_extract_expand(&state.ck, &ss[..]);
		state.ck = t1;
		t2
	}

	#[inline]
	fn outbound_noise_act<U: secp256k1::Signing>(
		secp_ctx: &Secp256k1<U>,
		state: &mut BidirectionalNoiseState,
		our_key: &SecretKey,
		their_key: &PublicKey,
	) -> ([u8; 50], [u8; 32]) {
		let our_pub = PublicKey::from_secret_key(secp_ctx, &our_key);

		let mut sha = Sha256::engine();
		sha.input(&state.h);
		sha.input(&our_pub.serialize()[..]);
		state.h = Sha256::from_engine(sha).into_inner();

		let ss = SharedSecret::new(&their_key, &our_key);
		let temp_k = Self::hkdf(state, ss);

		let mut res = [0; 50];
		res[1..34].copy_from_slice(&our_pub.serialize()[..]);
		Self::encrypt_with_ad(&mut res[34..], 0, &temp_k, &state.h, &[0; 0]);

		let mut sha = Sha256::engine();
		sha.input(&state.h);
		sha.input(&res[34..]);
		state.h = Sha256::from_engine(sha).into_inner();

		(res, temp_k)
	}

	#[inline]
	fn inbound_noise_act(
		state: &mut BidirectionalNoiseState,
		act: &[u8],
		our_key: &SecretKey,
	) -> Result<(PublicKey, [u8; 32]), HandleError> {
		assert_eq!(act.len(), 50);

		if act[0] != 0 {
			return Err(HandleError {
				err: "Unknown handshake version number",
				action: Some(msgs::ErrorAction::DisconnectPeer { msg: None }),
			});
		}

		let their_pub = match PublicKey::from_slice(&act[1..34]) {
			Err(_) => {
				return Err(HandleError {
					err: "Invalid public key",
					action: Some(msgs::ErrorAction::DisconnectPeer { msg: None }),
				})
			}
			Ok(key) => key,
		};

		let mut sha = Sha256::engine();
		sha.input(&state.h);
		sha.input(&their_pub.serialize()[..]);
		state.h = Sha256::from_engine(sha).into_inner();

		let ss = SharedSecret::new(&their_pub, &our_key);
		let temp_k = Self::hkdf(state, ss);

		let mut dec = [0; 0];
		Self::decrypt_with_ad(&mut dec, 0, &temp_k, &state.h, &act[34..])?;

		let mut sha = Sha256::engine();
		sha.input(&state.h);
		sha.input(&act[34..]);
		state.h = Sha256::from_engine(sha).into_inner();

		Ok((their_pub, temp_k))
	}
}

impl PeerChannelEncryptor<InProgress<PreActOne<Outbound>>> {
	pub fn get_act_one(
		mut self,
	) -> (
		PeerChannelEncryptor<InProgress<PostActOne<Outbound>>>,
		[u8; 50],
	) {
		let (res, _) = Self::outbound_noise_act(
			&self.secp_ctx,
			&mut self.noise_state.bidirectional_state,
			&self.noise_state.directional_state.ie,
			&self.noise_state.directional_state.their_node_id,
		);
		(
			PeerChannelEncryptor {
				secp_ctx: self.secp_ctx,
				noise_state: InProgress {
					state: PhantomData,
					bidirectional_state: self.noise_state.bidirectional_state,
					directional_state: self.noise_state.directional_state,
				},
			},
			res,
		)
	}
}

impl PeerChannelEncryptor<InProgress<PreActOne<Inbound>>> {
	/// panics if act_one != 50 bytes
	pub fn process_act_one_with_keys(
		self,
		act_one: &[u8], // TODO: Use sized slices
		our_node_secret: &SecretKey,
		our_ephemeral: SecretKey,
	) -> Result<
		(
			PeerChannelEncryptor<InProgress<PostActTwo<Inbound>>>,
			[u8; 50],
		),
		HandleError,
	> {
		assert!(act_one.len() == 50);
		let (their_pub, _) = Self::inbound_noise_act(
			&mut self.noise_state.bidirectional_state,
			act_one,
			&our_node_secret,
		)?;
		let ie = their_pub;
		let re = our_ephemeral;

		let (res, temp_k) = Self::outbound_noise_act(
			&self.secp_ctx,
			&mut self.noise_state.bidirectional_state,
			&re,
			&ie,
		);
		let data = InboundPostActTwo {
			ie,
			re,
			temp_k2: temp_k,
		};
		Ok((
			PeerChannelEncryptor {
				secp_ctx: self.secp_ctx,
				noise_state: InProgress {
					state: PhantomData,
					bidirectional_state: self.noise_state.bidirectional_state,
					directional_state: data,
				},
			},
			res,
		))
	}
}

impl PeerChannelEncryptor<InProgress<PostActOne<Outbound>>> {
	/// panics if act_two != 50 bytes
	pub fn process_act_two(
		self,
		act_two: &[u8], // TODO: Use sized slices
		our_node_secret: &SecretKey,
	) -> Result<(PeerChannelEncryptor<Finished>, [u8; 66], PublicKey), HandleError> {
		assert!(act_two.len() == 50);
		let (re, temp_k2) = Self::inbound_noise_act(
			&mut self.noise_state.bidirectional_state,
			act_two,
			&self.noise_state.directional_state.ie,
		)?;

		let mut res = [0; 66];
		let our_node_id = PublicKey::from_secret_key(&self.secp_ctx, &our_node_secret);

		Self::encrypt_with_ad(
			&mut res[1..50],
			1,
			&temp_k2,
			&self.noise_state.bidirectional_state.h,
			&our_node_id.serialize()[..],
		);

		let mut sha = Sha256::engine();
		sha.input(&self.noise_state.bidirectional_state.h);
		sha.input(&res[1..50]);
		self.noise_state.bidirectional_state.h = Sha256::from_engine(sha).into_inner();

		let ss = SharedSecret::new(&re, our_node_secret);
		let temp_k = Self::hkdf(&mut self.noise_state.bidirectional_state, ss);

		Self::encrypt_with_ad(
			&mut res[50..],
			0,
			&temp_k,
			&self.noise_state.bidirectional_state.h,
			&[0; 0],
		);
		let final_hkdf =
			Self::hkdf_extract_expand(&self.noise_state.bidirectional_state.ck, &[0; 0]);
		let ck = self.noise_state.bidirectional_state.ck;

		let (sk, rk) = final_hkdf;
		let noise_state = Finished {
			sk: sk,
			sn: 0,
			sck: ck.clone(),
			rk: rk,
			rn: 0,
			rck: ck,
		};

		Ok((
			PeerChannelEncryptor {
				secp_ctx: self.secp_ctx,
				noise_state,
			},
			res,
			self.noise_state.directional_state.their_node_id,
		))
	}
}

impl PeerChannelEncryptor<InProgress<PostActTwo<Inbound>>> {
	/// panics if act_three != 66 bytes
	pub fn process_act_three(
		self,
		act_three: &[u8], // TODO: Use sized slices
	) -> Result<(PeerChannelEncryptor<Finished>, PublicKey), HandleError> {
		assert!(act_three.len() == 66);
		if act_three[0] != 0 {
			return Err(HandleError {
				err: "Unknown handshake version number",
				action: Some(msgs::ErrorAction::DisconnectPeer { msg: None }),
			});
		}

		let mut their_node_id = [0; 33];
		Self::decrypt_with_ad(
			&mut their_node_id,
			1,
			&self.noise_state.directional_state.temp_k2,
			&self.noise_state.bidirectional_state.h,
			&act_three[1..50],
		)?;
		let their_node_id = match PublicKey::from_slice(&their_node_id) {
			Ok(key) => key,
			Err(_) => {
				return Err(HandleError {
					err: "Bad node_id from peer",
					action: Some(msgs::ErrorAction::DisconnectPeer { msg: None }),
				})
			}
		};

		let mut sha = Sha256::engine();
		sha.input(&self.noise_state.bidirectional_state.h);
		sha.input(&act_three[1..50]);
		self.noise_state.bidirectional_state.h = Sha256::from_engine(sha).into_inner();

		let ss = SharedSecret::new(&their_node_id, &self.noise_state.directional_state.re);
		let temp_k = Self::hkdf(&mut self.noise_state.bidirectional_state, ss);

		Self::decrypt_with_ad(
			&mut [0; 0],
			0,
			&temp_k,
			&self.noise_state.bidirectional_state.h,
			&act_three[50..],
		)?;
		let final_hkdf =
			Self::hkdf_extract_expand(&self.noise_state.bidirectional_state.ck, &[0; 0]);
		let ck = self.noise_state.bidirectional_state.ck;

		let (rk, sk) = final_hkdf;
		let noise_state = Finished {
			sk: sk,
			sn: 0,
			sck: ck.clone(),
			rk: rk,
			rn: 0,
			rck: ck,
		};

		Ok((
			PeerChannelEncryptor {
				secp_ctx: self.secp_ctx,
				noise_state,
			},
			their_node_id,
		))
	}
}

impl PeerChannelEncryptor<Finished> {
	/// Encrypts the given message, returning the encrypted version
	/// panics if msg.len() > 65535.
	pub fn encrypt_message(&mut self, msg: &[u8]) -> Vec<u8> {
		if msg.len() > 65535 {
			panic!("Attempted to encrypt message longer than 65535 bytes!");
		}

		let mut res = Vec::with_capacity(msg.len() + 16 * 2 + 2);
		res.resize(msg.len() + 16 * 2 + 2, 0);

		match self.noise_state {
			Finished {
				ref mut sk,
				ref mut sn,
				ref mut sck,
				rk: _,
				rn: _,
				rck: _,
			} => {
				if *sn >= 1000 {
					let (new_sck, new_sk) = Self::hkdf_extract_expand(sck, sk);
					*sck = new_sck;
					*sk = new_sk;
					*sn = 0;
				}

				Self::encrypt_with_ad(
					&mut res[0..16 + 2],
					*sn,
					sk,
					&[0; 0],
					&byte_utils::be16_to_array(msg.len() as u16),
				);
				*sn += 1;

				Self::encrypt_with_ad(&mut res[16 + 2..], *sn, sk, &[0; 0], msg);
				*sn += 1;
			}
		}

		res
	}

	/// Decrypts a message length header from the remote peer.
	/// panics if noise handshake has not yet finished or msg.len() != 18
	pub fn decrypt_length_header(&mut self, msg: &[u8]) -> Result<u16, HandleError> {
		assert_eq!(msg.len(), 16 + 2);

		match self.noise_state {
			Finished {
				sk: _,
				sn: _,
				sck: _,
				ref mut rk,
				ref mut rn,
				ref mut rck,
			} => {
				if *rn >= 1000 {
					let (new_rck, new_rk) = Self::hkdf_extract_expand(rck, rk);
					*rck = new_rck;
					*rk = new_rk;
					*rn = 0;
				}

				let mut res = [0; 2];
				Self::decrypt_with_ad(&mut res, *rn, rk, &[0; 0], msg)?;
				*rn += 1;
				Ok(byte_utils::slice_to_be16(&res))
			}
		}
	}

	/// Decrypts the given message.
	/// panics if msg.len() > 65535 + 16
	pub fn decrypt_message(&mut self, msg: &[u8]) -> Result<Vec<u8>, HandleError> {
		if msg.len() > 65535 + 16 {
			panic!("Attempted to encrypt message longer than 65535 bytes!");
		}

		match self.noise_state {
			Finished {
				sk: _,
				sn: _,
				sck: _,
				ref rk,
				ref mut rn,
				rck: _,
			} => {
				let mut res = Vec::with_capacity(msg.len() - 16);
				res.resize(msg.len() - 16, 0);
				Self::decrypt_with_ad(&mut res[..], *rn, rk, &[0; 0], msg)?;
				*rn += 1;

				Ok(res)
			}
		}
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	use secp256k1::key::{PublicKey, SecretKey};

	use hex;

	use ln::peer_channel_encryptor::{NoiseState, PeerChannelEncryptor};

	use std::convert::TryInto;

	fn get_outbound_peer_for_initiator_test_vectors(
	) -> PeerChannelEncryptor<InProgress<PostActOne<Outbound>>> {
		let their_node_id = PublicKey::from_slice(
			&hex::decode("028d7500dd4c12685d1f568b4c2b5048e8534b873319f3a8daa612b469132ec7f7")
				.unwrap()[..],
		)
		.unwrap();

		let outbound_peer = PeerChannelEncryptor::new_outbound(
			their_node_id,
			SecretKey::from_slice(
				&hex::decode("1212121212121212121212121212121212121212121212121212121212121212")
					.unwrap()[..],
			)
			.unwrap(),
		);
		let (outbound_peer, act_one) = outbound_peer.get_act_one();
		assert_eq!(act_one[..], hex::decode("00036360e856310ce5d294e8be33fc807077dc56ac80d95d9cd4ddbd21325eff73f70df6086551151f58b8afe6c195782c6a").unwrap()[..]);
		outbound_peer
	}

	#[test]
	fn noise_initiator_test_vectors() {
		let our_node_id = SecretKey::from_slice(
			&hex::decode("1111111111111111111111111111111111111111111111111111111111111111")
				.unwrap()[..],
		)
		.unwrap();

		{
			// transport-initiator successful handshake
			let outbound_peer = get_outbound_peer_for_initiator_test_vectors();

			let act_two = hex::decode("0002466d7fcae563e5cb09a0d1870bb580344804617879a14949cf22285f1bae3f276e2470b93aac583c9ef6eafca3f730ae").unwrap().to_vec();
			let (outbound_peer, act_three, _) = outbound_peer
				.process_act_two((&act_two[..]).try_into().unwrap(), &our_node_id)
				.unwrap();
			assert_eq!(act_three[..], hex::decode("00b9e3a702e93e3a9948c2ed6e5fd7590a6e1c3a0344cfc9d5b57357049aa22355361aa02e55a8fc28fef5bd6d71ad0c38228dc68b1c466263b47fdf31e560e139ba").unwrap()[..]);

			match outbound_peer.noise_state {
				Finished {
					sk,
					sn,
					sck,
					rk,
					rn,
					rck,
				} => {
					assert_eq!(
						sk,
						hex::decode(
							"969ab31b4d288cedf6218839b27a3e2140827047f2c0f01bf5c04435d43511a9"
						)
						.unwrap()[..]
					);
					assert_eq!(sn, 0);
					assert_eq!(
						sck,
						hex::decode(
							"919219dbb2920afa8db80f9a51787a840bcf111ed8d588caf9ab4be716e42b01"
						)
						.unwrap()[..]
					);
					assert_eq!(
						rk,
						hex::decode(
							"bb9020b8965f4df047e07f955f3c4b88418984aadc5cdb35096b9ea8fa5c3442"
						)
						.unwrap()[..]
					);
					assert_eq!(rn, 0);
					assert_eq!(
						rck,
						hex::decode(
							"919219dbb2920afa8db80f9a51787a840bcf111ed8d588caf9ab4be716e42b01"
						)
						.unwrap()[..]
					);
				}
			}
		}
		{
			// transport-initiator act2 short read test
			// Can't actually test this cause process_act_two requires you pass the right length!
		}
		{
			// transport-initiator act2 bad version test
			let mut outbound_peer = get_outbound_peer_for_initiator_test_vectors();

			let act_two = hex::decode("0102466d7fcae563e5cb09a0d1870bb580344804617879a14949cf22285f1bae3f276e2470b93aac583c9ef6eafca3f730ae").unwrap().to_vec();
			assert!(outbound_peer
				.process_act_two(&act_two[..], &our_node_id)
				.is_err());
		}

		{
			// transport-initiator act2 bad key serialization test
			let mut outbound_peer = get_outbound_peer_for_initiator_test_vectors();

			let act_two = hex::decode("0004466d7fcae563e5cb09a0d1870bb580344804617879a14949cf22285f1bae3f276e2470b93aac583c9ef6eafca3f730ae").unwrap().to_vec();
			assert!(outbound_peer
				.process_act_two(&act_two[..], &our_node_id)
				.is_err());
		}

		{
			// transport-initiator act2 bad MAC test
			let mut outbound_peer = get_outbound_peer_for_initiator_test_vectors();

			let act_two = hex::decode("0002466d7fcae563e5cb09a0d1870bb580344804617879a14949cf22285f1bae3f276e2470b93aac583c9ef6eafca3f730af").unwrap().to_vec();
			assert!(outbound_peer
				.process_act_two(&act_two[..], &our_node_id)
				.is_err());
		}
	}

	#[test]
	fn noise_responder_test_vectors() {
		let our_node_id = SecretKey::from_slice(
			&hex::decode("2121212121212121212121212121212121212121212121212121212121212121")
				.unwrap()[..],
		)
		.unwrap();
		let our_ephemeral = SecretKey::from_slice(
			&hex::decode("2222222222222222222222222222222222222222222222222222222222222222")
				.unwrap()[..],
		)
		.unwrap();

		{
			// transport-responder successful handshake
			let inbound_peer = PeerChannelEncryptor::new_inbound(&our_node_id);

			let act_one = hex::decode("00036360e856310ce5d294e8be33fc807077dc56ac80d95d9cd4ddbd21325eff73f70df6086551151f58b8afe6c195782c6a").unwrap().to_vec();
			let (inbound_peer, act_two) = inbound_peer
				.process_act_one_with_keys(&act_one[..], &our_node_id, our_ephemeral.clone())
				.unwrap();
			assert_eq!(act_two[..], hex::decode("0002466d7fcae563e5cb09a0d1870bb580344804617879a14949cf22285f1bae3f276e2470b93aac583c9ef6eafca3f730ae").unwrap()[..]);

			let act_three = hex::decode("00b9e3a702e93e3a9948c2ed6e5fd7590a6e1c3a0344cfc9d5b57357049aa22355361aa02e55a8fc28fef5bd6d71ad0c38228dc68b1c466263b47fdf31e560e139ba").unwrap().to_vec();
			// test vector doesn't specify the initiator static key, but it's the same as the one
			// from transport-initiator successful handshake
			let (inbound_peer, pubkey) = inbound_peer.process_act_three(&act_three[..]).unwrap();
			assert_eq!(
				pubkey.serialize()[..],
				hex::decode("034f355bdcb7cc0af728ef3cceb9615d90684bb5b2ca5f859ab0f0b704075871aa")
					.unwrap()[..]
			);

			match inbound_peer.noise_state {
				Finished {
					sk,
					sn,
					sck,
					rk,
					rn,
					rck,
				} => {
					assert_eq!(
						sk,
						hex::decode(
							"bb9020b8965f4df047e07f955f3c4b88418984aadc5cdb35096b9ea8fa5c3442"
						)
						.unwrap()[..]
					);
					assert_eq!(sn, 0);
					assert_eq!(
						sck,
						hex::decode(
							"919219dbb2920afa8db80f9a51787a840bcf111ed8d588caf9ab4be716e42b01"
						)
						.unwrap()[..]
					);
					assert_eq!(
						rk,
						hex::decode(
							"969ab31b4d288cedf6218839b27a3e2140827047f2c0f01bf5c04435d43511a9"
						)
						.unwrap()[..]
					);
					assert_eq!(rn, 0);
					assert_eq!(
						rck,
						hex::decode(
							"919219dbb2920afa8db80f9a51787a840bcf111ed8d588caf9ab4be716e42b01"
						)
						.unwrap()[..]
					);
				}
			}
		}
		{
			// transport-responder act1 short read test
			// Can't actually test this cause process_act_one requires you pass the right length!
		}
		{
			// transport-responder act1 bad version test
			let mut inbound_peer = PeerChannelEncryptor::new_inbound(&our_node_id);

			let act_one = hex::decode("01036360e856310ce5d294e8be33fc807077dc56ac80d95d9cd4ddbd21325eff73f70df6086551151f58b8afe6c195782c6a").unwrap().to_vec();
			assert!(inbound_peer
				.process_act_one_with_keys(&act_one[..], &our_node_id, our_ephemeral.clone())
				.is_err());
		}
		{
			// transport-responder act1 bad key serialization test
			let mut inbound_peer = PeerChannelEncryptor::new_inbound(&our_node_id);

			let act_one =hex::decode("00046360e856310ce5d294e8be33fc807077dc56ac80d95d9cd4ddbd21325eff73f70df6086551151f58b8afe6c195782c6a").unwrap().to_vec();
			assert!(inbound_peer
				.process_act_one_with_keys(&act_one[..], &our_node_id, our_ephemeral.clone())
				.is_err());
		}
		{
			// transport-responder act1 bad MAC test
			let mut inbound_peer = PeerChannelEncryptor::new_inbound(&our_node_id);

			let act_one = hex::decode("00036360e856310ce5d294e8be33fc807077dc56ac80d95d9cd4ddbd21325eff73f70df6086551151f58b8afe6c195782c6b").unwrap().to_vec();
			assert!(inbound_peer
				.process_act_one_with_keys(&act_one[..], &our_node_id, our_ephemeral.clone())
				.is_err());
		}
		{
			// transport-responder act3 bad version test
			let inbound_peer = PeerChannelEncryptor::new_inbound(&our_node_id);

			let act_one = hex::decode("00036360e856310ce5d294e8be33fc807077dc56ac80d95d9cd4ddbd21325eff73f70df6086551151f58b8afe6c195782c6a").unwrap().to_vec();
			let (inbound_peer, act_two) = inbound_peer
				.process_act_one_with_keys(&act_one[..], &our_node_id, our_ephemeral.clone())
				.unwrap();
			assert_eq!(act_two[..], hex::decode("0002466d7fcae563e5cb09a0d1870bb580344804617879a14949cf22285f1bae3f276e2470b93aac583c9ef6eafca3f730ae").unwrap()[..]);

			let act_three = hex::decode("01b9e3a702e93e3a9948c2ed6e5fd7590a6e1c3a0344cfc9d5b57357049aa22355361aa02e55a8fc28fef5bd6d71ad0c38228dc68b1c466263b47fdf31e560e139ba").unwrap().to_vec();
			assert!(inbound_peer.process_act_three(&act_three[..]).is_err());
		}
		{
			// transport-responder act3 short read test
			// Can't actually test this cause process_act_three requires you pass the right length!
		}
		{
			// transport-responder act3 bad MAC for ciphertext test
			let inbound_peer = PeerChannelEncryptor::new_inbound(&our_node_id);

			let act_one = hex::decode("00036360e856310ce5d294e8be33fc807077dc56ac80d95d9cd4ddbd21325eff73f70df6086551151f58b8afe6c195782c6a").unwrap().to_vec();
			let (inbound_peer, act_two) = inbound_peer
				.process_act_one_with_keys(&act_one[..], &our_node_id, our_ephemeral.clone())
				.unwrap();
			assert_eq!(act_two[..], hex::decode("0002466d7fcae563e5cb09a0d1870bb580344804617879a14949cf22285f1bae3f276e2470b93aac583c9ef6eafca3f730ae").unwrap()[..]);

			let act_three = hex::decode("00c9e3a702e93e3a9948c2ed6e5fd7590a6e1c3a0344cfc9d5b57357049aa22355361aa02e55a8fc28fef5bd6d71ad0c38228dc68b1c466263b47fdf31e560e139ba").unwrap().to_vec();
			assert!(inbound_peer.process_act_three(&act_three[..]).is_err());
		}
		{
			// transport-responder act3 bad rs test
			let inbound_peer = PeerChannelEncryptor::new_inbound(&our_node_id);

			let act_one = hex::decode("00036360e856310ce5d294e8be33fc807077dc56ac80d95d9cd4ddbd21325eff73f70df6086551151f58b8afe6c195782c6a").unwrap().to_vec();
			let (inbound_peer, act_two) = inbound_peer
				.process_act_one_with_keys(&act_one[..], &our_node_id, our_ephemeral.clone())
				.unwrap();
			assert_eq!(act_two[..], hex::decode("0002466d7fcae563e5cb09a0d1870bb580344804617879a14949cf22285f1bae3f276e2470b93aac583c9ef6eafca3f730ae").unwrap()[..]);

			let act_three = hex::decode("00bfe3a702e93e3a9948c2ed6e5fd7590a6e1c3a0344cfc9d5b57357049aa2235536ad09a8ee351870c2bb7f78b754a26c6cef79a98d25139c856d7efd252c2ae73c").unwrap().to_vec();
			assert!(inbound_peer.process_act_three(&act_three[..]).is_err());
		}
		{
			// transport-responder act3 bad MAC test
			let inbound_peer = PeerChannelEncryptor::new_inbound(&our_node_id);

			let act_one = hex::decode("00036360e856310ce5d294e8be33fc807077dc56ac80d95d9cd4ddbd21325eff73f70df6086551151f58b8afe6c195782c6a").unwrap().to_vec();
			let (inbound_peer, act_two) = inbound_peer
				.process_act_one_with_keys(&act_one[..], &our_node_id, our_ephemeral.clone())
				.unwrap();
			assert_eq!(act_two[..], hex::decode("0002466d7fcae563e5cb09a0d1870bb580344804617879a14949cf22285f1bae3f276e2470b93aac583c9ef6eafca3f730ae").unwrap()[..]);

			let act_three = hex::decode("00b9e3a702e93e3a9948c2ed6e5fd7590a6e1c3a0344cfc9d5b57357049aa22355361aa02e55a8fc28fef5bd6d71ad0c38228dc68b1c466263b47fdf31e560e139bb").unwrap().to_vec();
			assert!(inbound_peer.process_act_three(&act_three[..]).is_err());
		}
	}

	#[test]
	fn message_encryption_decryption_test_vectors() {
		// We use the same keys as the initiator and responder test vectors, so we copy those tests
		// here and use them to encrypt.
		let outbound_peer = get_outbound_peer_for_initiator_test_vectors();

		let mut outbound_peer = {
			let our_node_id = SecretKey::from_slice(
				&hex::decode("1111111111111111111111111111111111111111111111111111111111111111")
					.unwrap()[..],
			)
			.unwrap();

			let act_two = hex::decode("0002466d7fcae563e5cb09a0d1870bb580344804617879a14949cf22285f1bae3f276e2470b93aac583c9ef6eafca3f730ae").unwrap().to_vec();
			let (outbound_peer, act_three, pubkey) = outbound_peer
				.process_act_two(&act_two[..], &our_node_id)
				.unwrap();
			assert_eq!(act_three[..], hex::decode("00b9e3a702e93e3a9948c2ed6e5fd7590a6e1c3a0344cfc9d5b57357049aa22355361aa02e55a8fc28fef5bd6d71ad0c38228dc68b1c466263b47fdf31e560e139ba").unwrap()[..]);

			match outbound_peer.noise_state {
				Finished {
					sk,
					sn,
					sck,
					rk,
					rn,
					rck,
				} => {
					assert_eq!(
						sk,
						hex::decode(
							"969ab31b4d288cedf6218839b27a3e2140827047f2c0f01bf5c04435d43511a9"
						)
						.unwrap()[..]
					);
					assert_eq!(sn, 0);
					assert_eq!(
						sck,
						hex::decode(
							"919219dbb2920afa8db80f9a51787a840bcf111ed8d588caf9ab4be716e42b01"
						)
						.unwrap()[..]
					);
					assert_eq!(
						rk,
						hex::decode(
							"bb9020b8965f4df047e07f955f3c4b88418984aadc5cdb35096b9ea8fa5c3442"
						)
						.unwrap()[..]
					);
					assert_eq!(rn, 0);
					assert_eq!(
						rck,
						hex::decode(
							"919219dbb2920afa8db80f9a51787a840bcf111ed8d588caf9ab4be716e42b01"
						)
						.unwrap()[..]
					);
				}
			};
			outbound_peer
		};

		let mut inbound_peer = {
			// transport-responder successful handshake
			let our_node_id = SecretKey::from_slice(
				&hex::decode("2121212121212121212121212121212121212121212121212121212121212121")
					.unwrap()[..],
			)
			.unwrap();
			let our_ephemeral = SecretKey::from_slice(
				&hex::decode("2222222222222222222222222222222222222222222222222222222222222222")
					.unwrap()[..],
			)
			.unwrap();

			let inbound_peer = PeerChannelEncryptor::new_inbound(&our_node_id);

			let act_one = hex::decode("00036360e856310ce5d294e8be33fc807077dc56ac80d95d9cd4ddbd21325eff73f70df6086551151f58b8afe6c195782c6a").unwrap().to_vec();
			let (inbound_peer, act_two) = inbound_peer
				.process_act_one_with_keys(&act_one[..], &our_node_id, our_ephemeral.clone())
				.unwrap();
			assert_eq!(act_two[..], hex::decode("0002466d7fcae563e5cb09a0d1870bb580344804617879a14949cf22285f1bae3f276e2470b93aac583c9ef6eafca3f730ae").unwrap()[..]);

			let act_three = hex::decode("00b9e3a702e93e3a9948c2ed6e5fd7590a6e1c3a0344cfc9d5b57357049aa22355361aa02e55a8fc28fef5bd6d71ad0c38228dc68b1c466263b47fdf31e560e139ba").unwrap().to_vec();
			// test vector doesn't specify the initiator static key, but it's the same as the one
			// from transport-initiator successful handshake
			let (inbound_peer, pubkey) = inbound_peer.process_act_three(&act_three[..]).unwrap();
			assert_eq!(
				pubkey.serialize()[..],
				hex::decode("034f355bdcb7cc0af728ef3cceb9615d90684bb5b2ca5f859ab0f0b704075871aa")
					.unwrap()[..]
			);

			match inbound_peer.noise_state {
				Finished {
					sk,
					sn,
					sck,
					rk,
					rn,
					rck,
				} => {
					assert_eq!(
						sk,
						hex::decode(
							"bb9020b8965f4df047e07f955f3c4b88418984aadc5cdb35096b9ea8fa5c3442"
						)
						.unwrap()[..]
					);
					assert_eq!(sn, 0);
					assert_eq!(
						sck,
						hex::decode(
							"919219dbb2920afa8db80f9a51787a840bcf111ed8d588caf9ab4be716e42b01"
						)
						.unwrap()[..]
					);
					assert_eq!(
						rk,
						hex::decode(
							"969ab31b4d288cedf6218839b27a3e2140827047f2c0f01bf5c04435d43511a9"
						)
						.unwrap()[..]
					);
					assert_eq!(rn, 0);
					assert_eq!(
						rck,
						hex::decode(
							"919219dbb2920afa8db80f9a51787a840bcf111ed8d588caf9ab4be716e42b01"
						)
						.unwrap()[..]
					);
				}
			};
			inbound_peer
		};

		for i in 0..1005 {
			let msg = [0x68, 0x65, 0x6c, 0x6c, 0x6f];
			let res = outbound_peer.encrypt_message(&msg);
			assert_eq!(res.len(), 5 + 2 * 16 + 2);

			let len_header = res[0..2 + 16].to_vec();
			assert_eq!(
				inbound_peer.decrypt_length_header(&len_header[..]).unwrap() as usize,
				msg.len()
			);
			assert_eq!(
				inbound_peer.decrypt_message(&res[2 + 16..]).unwrap()[..],
				msg[..]
			);

			if i == 0 {
				assert_eq!(res, hex::decode("cf2b30ddf0cf3f80e7c35a6e6730b59fe802473180f396d88a8fb0db8cbcf25d2f214cf9ea1d95").unwrap());
			} else if i == 1 {
				assert_eq!(res, hex::decode("72887022101f0b6753e0c7de21657d35a4cb2a1f5cde2650528bbc8f837d0f0d7ad833b1a256a1").unwrap());
			} else if i == 500 {
				assert_eq!(res, hex::decode("178cb9d7387190fa34db9c2d50027d21793c9bc2d40b1e14dcf30ebeeeb220f48364f7a4c68bf8").unwrap());
			} else if i == 501 {
				assert_eq!(res, hex::decode("1b186c57d44eb6de4c057c49940d79bb838a145cb528d6e8fd26dbe50a60ca2c104b56b60e45bd").unwrap());
			} else if i == 1000 {
				assert_eq!(res, hex::decode("4a2f3cc3b5e78ddb83dcb426d9863d9d9a723b0337c89dd0b005d89f8d3c05c52b76b29b740f09").unwrap());
			} else if i == 1001 {
				assert_eq!(res, hex::decode("2ecd8c8a5629d0d02ab457a0fdd0f7b90a192cd46be5ecb6ca570bfc5e268338b1a16cf4ef2d36").unwrap());
			}
		}
	}
}
