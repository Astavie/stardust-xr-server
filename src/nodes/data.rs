use super::alias::AliasInfo;
use super::fields::Field;
use super::spatial::{parse_transform, Spatial};
use super::{Alias, Node};
use crate::core::client::Client;
use crate::core::nodelist::LifeLinkedNodeList;
use crate::core::registry::Registry;
use crate::nodes::fields::find_field;
use crate::nodes::spatial::find_spatial_parent;
use anyhow::{anyhow, ensure, Result};
use glam::vec3a;
use parking_lot::Mutex;
use serde::Deserialize;
use stardust_xr::schemas::flex::{deserialize, serialize};
use stardust_xr::values::Transform;
use std::sync::{Arc, Weak};

static PULSE_SENDER_REGISTRY: Registry<PulseSender> = Registry::new();
static PULSE_RECEIVER_REGISTRY: Registry<PulseReceiver> = Registry::new();

fn mask_matches(mask_map_lesser: &Mask, mask_map_greater: &Mask) -> bool {
	(|| -> Result<_> {
		for key in mask_map_lesser.get_mask()?.iter_keys() {
			let lesser_key_type = mask_map_lesser.get_mask()?.index(key)?.flexbuffer_type();
			let greater_key_type = mask_map_greater.get_mask()?.index(key)?.flexbuffer_type();
			if lesser_key_type != greater_key_type {
				return Err(flexbuffers::ReaderError::InvalidPackedType {}.into());
			}
		}
		Ok(())
	})()
	.is_ok()
}

type MaskMapGetFn = fn(&[u8]) -> Result<flexbuffers::MapReader<&[u8]>>;
pub struct Mask {
	binary: Vec<u8>,
	get_fn: MaskMapGetFn,
}
impl Mask {
	pub fn get_mask(&self) -> Result<flexbuffers::MapReader<&[u8]>> {
		(self.get_fn)(self.binary.as_slice())
	}
	pub fn set_mask(&mut self, binary: Vec<u8>, get_fn: MaskMapGetFn) {
		self.binary = binary;
		self.get_fn = get_fn;
	}
}
impl Default for Mask {
	fn default() -> Self {
		Mask {
			binary: Default::default(),
			get_fn: mask_get_err,
		}
	}
}
fn mask_get_err(_binary: &[u8]) -> Result<flexbuffers::MapReader<&[u8]>> {
	Err(anyhow!("You need to call setMask to set the mask!"))
}
fn mask_get_map_at_root(binary: &[u8]) -> Result<flexbuffers::MapReader<&[u8]>> {
	flexbuffers::Reader::get_root(binary)
		.map_err(|_| anyhow!("Mask is not a valid flexbuffer"))?
		.get_map()
		.map_err(|_| anyhow!("Mask is not a valid map"))
}

#[derive(Default)]
pub struct PulseSender {
	mask: Mutex<Mask>,
	aliases: LifeLinkedNodeList,
}
impl PulseSender {
	pub fn add_to(node: &Arc<Node>) -> Result<()> {
		ensure!(
			node.spatial.get().is_some(),
			"Internal: Node does not have a spatial attached!"
		);

		let sender = Default::default();
		let sender = PULSE_SENDER_REGISTRY.add(sender);
		let _ = node.pulse_sender.set(sender);
		node.add_local_signal("setMask", PulseSender::set_mask_flex);
		node.add_local_method("getReceivers", PulseSender::get_receivers_flex);
		Ok(())
	}
	pub fn set_mask_flex(node: &Node, _calling_client: Arc<Client>, data: &[u8]) -> Result<()> {
		ensure!(
			node.pulse_sender.get().is_some(),
			"Internal: Node does not have a pulse sender aspect"
		);
		node.pulse_sender
			.get()
			.unwrap()
			.mask
			.lock()
			.set_mask(data.to_vec(), mask_get_map_at_root);
		Ok(())
	}
	fn get_receivers_flex(
		node: &Node,
		calling_client: Arc<Client>,
		_data: &[u8],
	) -> Result<Vec<u8>> {
		let sender_spatial = node
			.spatial
			.get()
			.ok_or_else(|| anyhow!("Node does not have a spatial aspect!"))?;
		let sender = node
			.pulse_sender
			.get()
			.ok_or_else(|| anyhow!("Node does not have a sender aspect!"))?;
		let valid_receivers = PULSE_RECEIVER_REGISTRY.get_valid_contents();
		let mut distance_sorted_receivers: Vec<(f32, &PulseReceiver)> = valid_receivers
			.iter()
			.filter(|receiver| receiver.get_field().is_some())
			.filter(|receiver| mask_matches(&*sender.mask.lock(), &*receiver.mask.lock()))
			.map(|receiver| {
				(
					receiver
						.get_field()
						.unwrap()
						.distance(sender_spatial, vec3a(0_f32, 0_f32, 0_f32)),
					receiver.as_ref(),
				)
			})
			.collect();
		distance_sorted_receivers.sort_by(|(d1, _), (d2, _)| d1.partial_cmp(d2).unwrap());
		sender.aliases.clear();
		let uids: Vec<String> = distance_sorted_receivers
			.into_iter()
			.map(|(_, receiver)| {
				let receiver_alias = Alias::new(
					&calling_client,
					node.get_path(),
					receiver.uid.as_str(),
					receiver.node.upgrade().as_ref().unwrap(),
					AliasInfo {
						local_methods: vec!["sendData"],
						..Default::default()
					},
				);
				sender.aliases.add(Arc::downgrade(&receiver_alias));

				receiver.uid.clone()
			})
			.collect();

		serialize(uids).map_err(|e| e.into())
	}
}
impl Drop for PulseSender {
	fn drop(&mut self) {
		PULSE_SENDER_REGISTRY.remove(self);
	}
}

pub struct PulseReceiver {
	uid: String,
	node: Weak<Node>,
	pub mask: Mutex<Mask>,
	field: Weak<Field>,
}
impl PulseReceiver {
	pub fn add_to(node: &Arc<Node>, field: Arc<Field>) -> Result<()> {
		ensure!(
			node.spatial.get().is_some(),
			"Internal: Node does not have a spatial attached!"
		);

		let receiver = PulseReceiver {
			uid: node.uid.clone(),
			node: Arc::downgrade(node),
			field: Arc::downgrade(&field),
			mask: Default::default(),
		};
		let receiver = PULSE_RECEIVER_REGISTRY.add(receiver);
		let _ = node.pulse_receiver.set(receiver);
		node.add_local_signal("setMask", PulseReceiver::set_mask_flex);
		node.add_local_signal("sendData", PulseReceiver::send_data_flex);
		Ok(())
	}
	fn get_field(&self) -> Option<Arc<Field>> {
		self.field.upgrade()
	}
	fn send_data_flex(node: &Node, _calling_client: Arc<Client>, data: &[u8]) -> Result<()> {
		ensure!(
			node.pulse_receiver.get().is_some(),
			"Internal: Node does not have a pulse receiver aspect"
		);
		let receiver_mask = node.pulse_receiver.get().unwrap().mask.lock();
		let data_mask = Mask {
			binary: data.to_vec(),
			get_fn: mask_get_map_at_root,
		};
		if !mask_matches(&receiver_mask, &data_mask) {
			return Err(anyhow!(
				"Message does not contain the same keys as the receiver mask"
			));
		}
		drop(receiver_mask);
		node.send_remote_signal("pulse", data)?;
		Ok(())
	}
	fn set_mask_flex(node: &Node, _calling_client: Arc<Client>, data: &[u8]) -> Result<()> {
		ensure!(
			node.pulse_receiver.get().is_some(),
			"Internal: Node does not have a pulse receiver aspect"
		);
		node.pulse_receiver
			.get()
			.unwrap()
			.mask
			.lock()
			.set_mask(data.to_vec(), mask_get_map_at_root);
		Ok(())
	}
}

impl Drop for PulseReceiver {
	fn drop(&mut self) {
		PULSE_RECEIVER_REGISTRY.remove(self);
	}
}

pub fn create_interface(client: &Arc<Client>) {
	let node = Node::create(client, "", "data", false);
	node.add_local_signal("createPulseSender", create_pulse_sender_flex);
	node.add_local_signal("createPulseReceiver", create_pulse_receiver_flex);
	node.add_to_scenegraph();
}

// pub fn mask_get_map_pulse_sender_create_args(mask: &Mask) -> Result<flexbuffers::MapReader<&[u8]>> {
// 	flexbuffers::Reader::get_root(mask.binary.as_slice())
// 		.map_err(|_| anyhow!("Mask is not a valid flexbuffer"))?
// 		.get_vector()?
// 		.index(4)?
// 		.get_map()
// 		.map_err(|_| anyhow!("Mask is not a valid map"))
// }
pub fn create_pulse_sender_flex(
	_node: &Node,
	calling_client: Arc<Client>,
	data: &[u8],
) -> Result<()> {
	#[derive(Deserialize)]
	struct CreatePulseSenderInfo<'a> {
		name: &'a str,
		parent_path: &'a str,
		transform: Transform,
	}
	let info: CreatePulseSenderInfo = deserialize(data)?;
	let node = Node::create(&calling_client, "/data/sender", info.name, true);
	let parent = find_spatial_parent(&calling_client, info.parent_path)?;
	let transform = parse_transform(info.transform, true, true, false)?;
	let node = node.add_to_scenegraph();
	Spatial::add_to(&node, Some(parent), transform, false)?;
	PulseSender::add_to(&node)?;
	Ok(())
}

// pub fn mask_get_map_pulse_receiver_create_args(
// 	mask: &Mask,
// ) -> Result<flexbuffers::MapReader<&[u8]>> {
// 	flexbuffers::Reader::get_root(mask.binary.as_slice())
// 		.map_err(|_| anyhow!("Mask is not a valid flexbuffer"))?
// 		.get_vector()?
// 		.index(5)?
// 		.get_map()
// 		.map_err(|_| anyhow!("Mask is not a valid map"))
// }
pub fn create_pulse_receiver_flex(
	_node: &Node,
	calling_client: Arc<Client>,
	data: &[u8],
) -> Result<()> {
	#[derive(Deserialize)]
	struct CreatePulseReceiverInfo<'a> {
		name: &'a str,
		parent_path: &'a str,
		transform: Transform,
		field_path: &'a str,
	}
	let info: CreatePulseReceiverInfo = deserialize(data)?;
	let node = Node::create(&calling_client, "/data/sender", info.name, true);
	let parent = find_spatial_parent(&calling_client, info.parent_path)?;
	let transform = parse_transform(info.transform, true, true, false)?;
	let field = find_field(&calling_client, info.field_path)?;

	let node = node.add_to_scenegraph();
	Spatial::add_to(&node, Some(parent), transform, false)?;
	PulseReceiver::add_to(&node, field)?;
	Ok(())
}
