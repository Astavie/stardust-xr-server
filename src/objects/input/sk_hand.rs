use crate::{
	core::client::INTERNAL_CLIENT,
	nodes::{
		input::{hand::Hand, InputMethod, InputType},
		spatial::Spatial,
		Node,
	},
};
use color_eyre::eyre::Result;
use glam::Mat4;
use nanoid::nanoid;
use stardust_xr::schemas::{
	flat::{Datamap, Hand as FlatHand, Joint},
	flex::flexbuffers,
};
use std::sync::Arc;
use stereokit::{ButtonState, HandJoint, Handed, StereoKitMultiThread};
use tracing::instrument;

fn convert_joint(joint: HandJoint) -> Joint {
	Joint {
		position: joint.position.into(),
		rotation: joint.orientation.into(),
		radius: joint.radius,
	}
}

pub struct SkHand {
	_node: Arc<Node>,
	input: Arc<InputMethod>,
	handed: Handed,
}
impl SkHand {
	pub fn new(handed: Handed) -> Result<Self> {
		let _node = Node::create(&INTERNAL_CLIENT, "", &nanoid!(), false).add_to_scenegraph()?;
		Spatial::add_to(&_node, None, Mat4::IDENTITY, false)?;
		let hand = InputType::Hand(Box::new(Hand {
			base: FlatHand {
				right: handed == Handed::Right,
				..Default::default()
			},
		}));
		let input = InputMethod::add_to(&_node, hand, None)?;
		Ok(SkHand {
			_node,
			input,
			handed,
		})
	}
	#[instrument(level = "debug", name = "Update Hand Input Method", skip_all)]
	pub fn update(&mut self, sk: &impl StereoKitMultiThread) {
		let sk_hand = sk.input_hand(self.handed);
		if let InputType::Hand(hand) = &mut *self.input.specialization.lock() {
			let controller = sk.input_controller(self.handed);
			*self.input.enabled.lock() = controller.tracked.contains(ButtonState::INACTIVE)
				&& sk_hand.tracked_state.contains(ButtonState::ACTIVE);
			if *self.input.enabled.lock() {
				hand.base.thumb.tip = convert_joint(sk_hand.fingers[0][4]);
				hand.base.thumb.distal = convert_joint(sk_hand.fingers[0][3]);
				hand.base.thumb.proximal = convert_joint(sk_hand.fingers[0][2]);
				hand.base.thumb.metacarpal = convert_joint(sk_hand.fingers[0][1]);

				for (finger, sk_finger) in [
					(&mut hand.base.index, sk_hand.fingers[1]),
					(&mut hand.base.middle, sk_hand.fingers[2]),
					(&mut hand.base.ring, sk_hand.fingers[3]),
					(&mut hand.base.little, sk_hand.fingers[4]),
				] {
					finger.tip = convert_joint(sk_finger[4]);
					finger.distal = convert_joint(sk_finger[3]);
					finger.intermediate = convert_joint(sk_finger[2]);
					finger.proximal = convert_joint(sk_finger[1]);
					finger.metacarpal = convert_joint(sk_finger[0]);
				}

				hand.base.palm.position = sk_hand.palm.position.into();
				hand.base.palm.rotation = sk_hand.palm.orientation.into();
				hand.base.palm.radius =
					(sk_hand.fingers[2][0].radius + sk_hand.fingers[2][1].radius) * 0.5;

				hand.base.wrist.position = sk_hand.wrist.position.into();
				hand.base.wrist.rotation = sk_hand.wrist.orientation.into();
				hand.base.wrist.radius =
					(sk_hand.fingers[0][0].radius + sk_hand.fingers[4][0].radius) * 0.5;

				hand.base.elbow = None;
			}
		}
		let mut fbb = flexbuffers::Builder::default();
		let mut map = fbb.start_map();
		map.push("grab_strength", sk_hand.grip_activation);
		map.push("pinch_strength", sk_hand.pinch_activation);
		map.end_map();
		*self.input.datamap.lock() = Datamap::new(fbb.take_buffer()).ok();
	}
}
