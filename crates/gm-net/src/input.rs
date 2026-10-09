//! Input datagrams (PROTOCOL.md 4): 1..=4 consecutive frames per datagram.

use crate::bits::{BitReader, BitWriter};
use crate::quant;
use crate::{Kind, NetError, read_header, write_header};

/// Button bits (PROTOCOL.md 4), shared with the simulation.
pub use gm_core::sim::buttons;

/// One tick of input, already quantized to wire precision so client and server simulate the
/// same numbers.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct InputFrame {
    pub buttons: u16,
    /// 0.1° steps, `0..3600`.
    pub yaw: u16,
    /// 0.1° steps, `0..=1800` (0 = looking straight up, 1800 = straight down).
    pub pitch: u16,
    pub forward: i8,
    pub side: i8,
    /// Ability slot activated this tick (1-based), 0 = none.
    pub ability: u8,
    /// The weapon in hand (MODES.md 3.7): 0 the primary, 1 the secondary, 2 the knife.
    pub held: u8,
    /// The body the frame's activation is aimed at (MODES.md 5.3); 0 = none.
    pub target: u32,
    /// The item cell a `USE` this tick is of (LOOK.md 3.2), 1-based; 0 = none named (the
    /// first cell's). v18.
    pub use_slot: u8,
}

impl InputFrame {
    pub const BITS: usize = 16 + 12 + 11 + 8 + 8 + 8 + 2 + 32 + 3;

    fn write(&self, w: &mut BitWriter) {
        w.write_bits(self.buttons as u64, 16);
        w.write_bits(self.yaw as u64, 12);
        w.write_bits(self.pitch as u64, 11);
        w.write_bits(self.forward as u8 as u64, 8);
        w.write_bits(self.side as u8 as u64, 8);
        w.write_bits(self.ability as u64, 8);
        w.write_bits(self.held as u64, 2);
        w.write_bits(self.target as u64, 32);
        w.write_bits(self.use_slot as u64, 3);
    }

    fn read(r: &mut BitReader<'_>) -> Result<InputFrame, NetError> {
        let f = InputFrame {
            buttons: r.read_bits(16)? as u16,
            yaw: r.read_bits(12)? as u16,
            pitch: r.read_bits(11)? as u16,
            forward: r.read_bits(8)? as u8 as i8,
            side: r.read_bits(8)? as u8 as i8,
            ability: r.read_bits(8)? as u8,
            held: r.read_bits(2)? as u8,
            target: r.read_bits(32)? as u32,
            use_slot: r.read_bits(3)? as u8,
        };
        if f.buttons & buttons::RESERVED != 0 {
            return Err(NetError::Malformed("reserved button bits set"));
        }
        if f.held > 2 {
            return Err(NetError::Malformed("held out of range"));
        }
        if f.use_slot as usize > gm_core::sim::BAR_CELLS {
            return Err(NetError::Malformed("use_slot out of range"));
        }
        if f.yaw >= 3600 {
            return Err(NetError::Malformed("yaw out of range"));
        }
        if f.pitch > 1800 {
            return Err(NetError::Malformed("pitch out of range"));
        }
        Ok(f)
    }

    /// Quantize a simulation input to wire precision.
    pub fn from_sim(input: &gm_core::sim::Input) -> InputFrame {
        InputFrame {
            buttons: input.buttons & !buttons::RESERVED,
            yaw: quant::yaw_to_wire(input.yaw),
            pitch: quant::pitch_to_wire(input.pitch),
            forward: quant::axis_to_wire(input.forward),
            side: quant::axis_to_wire(input.side),
            ability: input.ability,
            held: input.held.min(2),
            target: input.target,
            use_slot: input.use_slot.min(gm_core::sim::BAR_CELLS as u8),
        }
    }

    /// The simulation input both sides run: dequantized from the wire values.
    pub fn to_sim(&self) -> gm_core::sim::Input {
        gm_core::sim::Input {
            buttons: self.buttons,
            yaw: quant::wire_to_yaw(self.yaw),
            pitch: quant::wire_to_pitch(self.pitch),
            forward: quant::wire_to_axis(self.forward),
            side: quant::wire_to_axis(self.side),
            ability: self.ability,
            held: self.held,
            target: self.target,
            use_slot: self.use_slot,
        }
    }
}

pub const MAX_FRAMES: usize = 4;

/// One input datagram.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct InputDatagram {
    /// Newest server tick whose snapshot the client decoded; 0 = none.
    pub ack_tick: u32,
    /// Server tick the client is displaying for other entities; 0 = none.
    pub view_tick: u32,
    /// Client tick of `frames[0]`; frame `i` is tick `first_tick + i`.
    pub first_tick: u32,
    pub count: u8,
    pub frames: [InputFrame; MAX_FRAMES],
}

impl InputDatagram {
    pub fn new(ack_tick: u32, view_tick: u32, first_tick: u32) -> Self {
        InputDatagram {
            ack_tick,
            view_tick,
            first_tick,
            count: 0,
            frames: [InputFrame::default(); MAX_FRAMES],
        }
    }

    /// Append a frame; the tick is implied by position. Panics past `MAX_FRAMES`.
    pub fn push(&mut self, frame: InputFrame) {
        assert!(
            (self.count as usize) < MAX_FRAMES,
            "at most four frames per datagram"
        );
        self.frames[self.count as usize] = frame;
        self.count += 1;
    }

    /// `(client tick, frame)` pairs, oldest first.
    pub fn frames(&self) -> impl Iterator<Item = (u32, InputFrame)> + '_ {
        self.frames[..self.count as usize]
            .iter()
            .enumerate()
            .map(move |(i, f)| (self.first_tick.wrapping_add(i as u32), *f))
    }

    pub fn last_tick(&self) -> u32 {
        self.first_tick
            .wrapping_add(self.count.saturating_sub(1) as u32)
    }

    pub fn encode(&self) -> Vec<u8> {
        assert!(
            (1..=MAX_FRAMES).contains(&(self.count as usize)),
            "an input datagram carries 1..=4 frames"
        );
        let mut w = BitWriter::with_capacity(40);
        write_header(&mut w, Kind::Input);
        w.write_bits(self.ack_tick as u64, 32);
        w.write_bits(self.view_tick as u64, 32);
        w.write_bits((self.count - 1) as u64, 2);
        w.write_bits(self.first_tick as u64, 32);
        for f in &self.frames[..self.count as usize] {
            f.write(&mut w);
        }
        w.finish()
    }

    pub fn decode(bytes: &[u8]) -> Result<InputDatagram, NetError> {
        let mut r = BitReader::new(bytes);
        if read_header(&mut r)? != Kind::Input {
            return Err(NetError::Malformed("not an input datagram"));
        }
        let mut d = InputDatagram::new(r.read_bits(32)? as u32, r.read_bits(32)? as u32, 0);
        let count = r.read_bits(2)? as u8 + 1;
        d.first_tick = r.read_bits(32)? as u32;
        for _ in 0..count {
            d.push(InputFrame::read(&mut r)?);
        }
        Ok(d)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frame(i: u8) -> InputFrame {
        InputFrame {
            buttons: buttons::JUMP | buttons::PRIMARY,
            yaw: 900 + i as u16,
            pitch: 850,
            forward: 127,
            side: -64,
            ability: i,
            held: 1,
            target: 70_000 + i as u32,
            use_slot: i % 5,
        }
    }

    #[test]
    fn round_trip_four_frames() {
        let mut d = InputDatagram::new(1234, 1230, 5000);
        for i in 0..4 {
            d.push(frame(i));
        }
        let bytes = d.encode();
        // header 16 + ack 32 + view 32 + count 2 + first 32 + 4 * 100 = 514 bits = 65 bytes
        // (v18: the item cell's 3 bits a frame).
        assert_eq!(bytes.len(), 65);
        let back = InputDatagram::decode(&bytes).unwrap();
        assert_eq!(back, d);
        let ticks: Vec<u32> = back.frames().map(|(t, _)| t).collect();
        assert_eq!(ticks, [5000, 5001, 5002, 5003]);
        assert_eq!(back.last_tick(), 5003);
    }

    #[test]
    fn single_frame_is_small() {
        let mut d = InputDatagram::new(0, 0, 1);
        d.push(InputFrame::default());
        // header 16 + ack 32 + view 32 + count 2 + first 32 + 97 = 211 bits = 27 bytes.
        assert_eq!(d.encode().len(), 27);
    }

    #[test]
    fn rejects_garbage() {
        assert_eq!(InputDatagram::decode(&[]), Err(NetError::Overrun));
        // The version after this one: wrong whatever this one is.
        let other = crate::PROTOCOL_VERSION + 1;
        assert_eq!(
            InputDatagram::decode(&[other, 0, 0]),
            Err(NetError::Version(other))
        );
        assert_eq!(
            InputDatagram::decode(&[crate::PROTOCOL_VERSION, 7]),
            Err(NetError::Kind(7))
        );
        let mut d = InputDatagram::new(0, 0, 1);
        d.push(InputFrame {
            buttons: 0x8000,
            ..Default::default()
        });
        assert!(matches!(
            InputDatagram::decode(&d.encode()),
            Err(NetError::Malformed(_))
        ));
        let mut d = InputDatagram::new(0, 0, 1);
        d.push(InputFrame::default());
        let mut bytes = d.encode();
        bytes.truncate(bytes.len() - 1);
        assert_eq!(InputDatagram::decode(&bytes), Err(NetError::Overrun));
    }

    #[test]
    fn sim_round_trip_is_exact_after_one_quantization() {
        let sim = gm_core::sim::Input {
            buttons: buttons::JUMP | buttons::PRIMARY,
            yaw: 123.456,
            pitch: -12.34,
            forward: 0.5,
            side: -1.0,
            ability: 2,
            held: 2,
            target: 9,
            use_slot: 3,
        };
        let wire = InputFrame::from_sim(&sim);
        let back = wire.to_sim();
        assert!((back.yaw - 123.5).abs() < 1e-4);
        assert!((back.pitch - (-12.3)).abs() < 1e-4);
        assert!((back.forward - 0.5).abs() < 0.005);
        assert_eq!(back.side, -1.0);
        assert_eq!(back.buttons, sim.buttons);
        // Quantizing the dequantized value is a fixed point.
        assert_eq!(InputFrame::from_sim(&back), wire);
    }
}
