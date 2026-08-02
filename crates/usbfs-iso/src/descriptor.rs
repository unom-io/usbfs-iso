//! Just enough USB descriptor walking to find an endpoint and read its transfer parameters.
//!
//! This is deliberately *not* a descriptor library. It exists so the caller does not have to
//! hand-transcribe `wMaxPacketSize` and `bInterval` out of `lsusb` output — getting those wrong is
//! the difference between a clean stream and a silently mis-scheduled one. Class-specific parsing
//! (audio terminals, format types, channel maps) lives in `uac-host`, per design rule 1.
//!
//! Pure parsing over a byte slice, so it is tier-0 testable everywhere.

use crate::{Error, Result};

/// `bDescriptorType` for a device descriptor.
pub const DT_DEVICE: u8 = 0x01;
/// `bDescriptorType` for a configuration descriptor.
pub const DT_CONFIG: u8 = 0x02;
/// `bDescriptorType` for an interface descriptor.
pub const DT_INTERFACE: u8 = 0x04;
/// `bDescriptorType` for an endpoint descriptor.
pub const DT_ENDPOINT: u8 = 0x05;
/// `bDescriptorType` for an interface association descriptor.
pub const DT_INTERFACE_ASSOCIATION: u8 = 0x0b;

/// One descriptor in a configuration blob, as raw bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Raw<'a> {
    /// `bDescriptorType`.
    pub descriptor_type: u8,
    /// The whole descriptor including its two-byte header.
    pub bytes: &'a [u8],
}

/// Iterator over the type-length-value descriptor chain.
///
/// Stops at the first malformed length rather than looping or panicking: a device that returns a
/// truncated configuration blob is a real thing, and it must not take the process with it.
#[derive(Debug, Clone)]
pub struct Iter<'a> {
    rest: &'a [u8],
}

impl<'a> Iter<'a> {
    /// Walk a descriptor blob — the bytes `read(2)` returns from a usbfs device node.
    pub fn new(bytes: &'a [u8]) -> Iter<'a> {
        Iter { rest: bytes }
    }
}

impl<'a> Iterator for Iter<'a> {
    type Item = Raw<'a>;

    fn next(&mut self) -> Option<Raw<'a>> {
        if self.rest.len() < 2 {
            return None;
        }
        let len = self.rest[0] as usize;
        // A zero (or sub-header) length would make this loop forever.
        if len < 2 || len > self.rest.len() {
            self.rest = &[];
            return None;
        }
        let (head, tail) = self.rest.split_at(len);
        self.rest = tail;
        Some(Raw {
            descriptor_type: head[1],
            bytes: head,
        })
    }
}

/// Endpoint transfer type, from `bmAttributes` bits 1:0.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TransferType {
    /// Endpoint 0.
    Control,
    /// Isochronous — the whole point of this crate.
    Isochronous,
    /// Bulk.
    Bulk,
    /// Interrupt.
    Interrupt,
}

/// Isochronous synchronisation type, from `bmAttributes` bits 3:2.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SyncType {
    /// No synchronisation.
    None,
    /// Asynchronous — the device runs its own clock and reports rate via a feedback endpoint.
    Asynchronous,
    /// Adaptive — the device slaves to the rate the host feeds it. **No feedback endpoint to
    /// service**, which is why the DualSense case needs none of `decent`'s calibration machinery.
    Adaptive,
    /// Synchronous — locked to the bus SOF.
    Synchronous,
}

/// Transfer direction.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Direction {
    /// Host to device.
    Out,
    /// Device to host.
    In,
}

/// A parsed standard endpoint descriptor.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Endpoint {
    /// `bEndpointAddress`, direction bit included.
    pub address: u8,
    /// `bmAttributes`.
    pub attributes: u8,
    /// `wMaxPacketSize` **verbatim**, including the high-speed multiplier bits.
    ///
    /// Use [`Endpoint::bytes_per_interval`] for the figure that actually sizes a buffer.
    pub max_packet_size: u16,
    /// `bInterval`.
    pub interval: u8,
}

impl Endpoint {
    /// Direction of travel.
    pub fn direction(&self) -> Direction {
        if self.address & 0x80 != 0 {
            Direction::In
        } else {
            Direction::Out
        }
    }

    /// Transfer type.
    pub fn transfer_type(&self) -> TransferType {
        match self.attributes & 0x03 {
            0 => TransferType::Control,
            1 => TransferType::Isochronous,
            2 => TransferType::Bulk,
            _ => TransferType::Interrupt,
        }
    }

    /// Isochronous synchronisation type. Meaningless for other transfer types.
    pub fn sync_type(&self) -> SyncType {
        match (self.attributes >> 2) & 0x03 {
            0 => SyncType::None,
            1 => SyncType::Asynchronous,
            2 => SyncType::Adaptive,
            _ => SyncType::Synchronous,
        }
    }

    /// Payload bytes this endpoint can move in one service interval.
    ///
    /// For high-speed isochronous and interrupt endpoints `wMaxPacketSize` is **not** a plain byte
    /// count: bits 12:11 carry "additional transactions per microframe", so a descriptor reading
    /// `0x1188` means 2 x 0x188 bytes, not 0x1188 bytes. Sizing a ring off the raw field is a
    /// classic way to under-allocate by a factor of three on high-bandwidth endpoints.
    pub fn bytes_per_interval(&self) -> usize {
        let size = (self.max_packet_size & 0x07ff) as usize;
        let additional = ((self.max_packet_size >> 11) & 0x03) as usize;
        size * (1 + additional)
    }

    fn parse(bytes: &[u8]) -> Option<Endpoint> {
        // bLength, bDescriptorType, bEndpointAddress, bmAttributes, wMaxPacketSize, bInterval
        if bytes.len() < 7 {
            return None;
        }
        Some(Endpoint {
            address: bytes[2],
            attributes: bytes[3],
            max_packet_size: u16::from_le_bytes([bytes[4], bytes[5]]),
            interval: bytes[6],
        })
    }
}

/// A parsed standard interface descriptor.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Interface {
    /// `bInterfaceNumber`.
    pub number: u8,
    /// `bAlternateSetting`.
    pub alt_setting: u8,
    /// `bNumEndpoints`.
    pub num_endpoints: u8,
    /// `bInterfaceClass`.
    pub class: u8,
    /// `bInterfaceSubClass`.
    pub subclass: u8,
    /// `bInterfaceProtocol`.
    pub protocol: u8,
}

impl Interface {
    fn parse(bytes: &[u8]) -> Option<Interface> {
        if bytes.len() < 9 {
            return None;
        }
        Some(Interface {
            number: bytes[2],
            alt_setting: bytes[3],
            num_endpoints: bytes[4],
            class: bytes[5],
            subclass: bytes[6],
            protocol: bytes[7],
        })
    }
}

/// The device descriptor's identity fields.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Device {
    /// `bcdUSB`.
    pub usb_version: u16,
    /// `idVendor`.
    pub vendor_id: u16,
    /// `idProduct`.
    pub product_id: u16,
    /// `bcdDevice`.
    pub device_version: u16,
}

impl Device {
    /// Parse the device descriptor, which usbfs places at the head of the blob.
    pub fn parse(blob: &[u8]) -> Result<Device> {
        let d = Iter::new(blob)
            .find(|d| d.descriptor_type == DT_DEVICE)
            .ok_or(Error::MalformedDescriptor("no device descriptor"))?;
        if d.bytes.len() < 18 {
            return Err(Error::MalformedDescriptor("device descriptor truncated"));
        }
        Ok(Device {
            usb_version: u16::from_le_bytes([d.bytes[2], d.bytes[3]]),
            vendor_id: u16::from_le_bytes([d.bytes[8], d.bytes[9]]),
            product_id: u16::from_le_bytes([d.bytes[10], d.bytes[11]]),
            device_version: u16::from_le_bytes([d.bytes[12], d.bytes[13]]),
        })
    }
}

/// Every interface alternate setting in the blob, paired with its endpoints.
///
/// Descriptors are positional: endpoints belong to the interface descriptor that most recently
/// preceded them, and class-specific descriptors sit between the two. This walk keeps that
/// association, which is what lets a caller ask "what does interface 1 alt 1 actually expose?"
pub fn interfaces(blob: &[u8]) -> Vec<(Interface, Vec<Endpoint>)> {
    let mut out: Vec<(Interface, Vec<Endpoint>)> = Vec::new();
    for d in Iter::new(blob) {
        match d.descriptor_type {
            DT_INTERFACE => {
                if let Some(i) = Interface::parse(d.bytes) {
                    out.push((i, Vec::new()));
                }
            }
            DT_ENDPOINT => {
                if let (Some(e), Some(last)) = (Endpoint::parse(d.bytes), out.last_mut()) {
                    last.1.push(e);
                }
            }
            _ => {}
        }
    }
    out
}

/// Find one endpoint within a specific interface alternate setting.
pub fn find_endpoint(blob: &[u8], interface: u8, alt: u8, address: u8) -> Option<Endpoint> {
    interfaces(blob)
        .into_iter()
        .find(|(i, _)| i.number == interface && i.alt_setting == alt)
        .and_then(|(_, eps)| eps.into_iter().find(|e| e.address == address))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fixtures;

    #[test]
    fn walks_the_dualsense_blob_without_running_off_the_end() {
        let blob = fixtures::DUALSENSE_DESCRIPTORS;
        let all: Vec<_> = Iter::new(blob).collect();
        assert!(all.len() > 10);
        // Every descriptor's declared length must have been inside the blob.
        assert_eq!(
            all.iter().map(|d| d.bytes.len()).sum::<usize>(),
            blob.len(),
            "the walk must consume the blob exactly"
        );
    }

    #[test]
    fn a_zero_length_descriptor_terminates_instead_of_looping() {
        let evil = [0x00u8, 0x02, 0xff, 0xff];
        assert_eq!(Iter::new(&evil).count(), 0);
    }

    #[test]
    fn a_length_past_the_end_terminates_instead_of_panicking() {
        let evil = [0x40u8, 0x02, 0xff];
        assert_eq!(Iter::new(&evil).count(), 0);
    }

    #[test]
    fn finds_the_dualsense_audio_out_endpoint() {
        let ep = find_endpoint(fixtures::DUALSENSE_DESCRIPTORS, 1, 1, 0x01)
            .expect("interface 1 alt 1 endpoint 0x01");
        assert_eq!(ep.transfer_type(), TransferType::Isochronous);
        assert_eq!(ep.direction(), Direction::Out);
        // Adaptive sync is the gift in §3.2: the device slaves to our rate, so there is no
        // feedback endpoint to service.
        assert_eq!(ep.sync_type(), SyncType::Adaptive);
        assert_eq!(ep.max_packet_size, 392);
        assert_eq!(ep.bytes_per_interval(), 392);
        assert_eq!(ep.interval, 4);
    }

    #[test]
    fn high_speed_multiplier_bits_are_not_mistaken_for_size() {
        let ep = Endpoint {
            address: 0x01,
            attributes: 0x09,
            // 2 additional transactions per microframe of 0x188 bytes each.
            max_packet_size: (2 << 11) | 0x188,
            interval: 4,
        };
        assert_eq!(ep.max_packet_size & 0x07ff, 0x188);
        assert_eq!(ep.bytes_per_interval(), 3 * 0x188);
    }

    #[test]
    fn device_identity_parses() {
        let d = Device::parse(fixtures::DUALSENSE_DESCRIPTORS).unwrap();
        assert_eq!(d.vendor_id, 0x054c);
        assert_eq!(d.product_id, 0x0ce6);
    }

    #[test]
    fn interface_and_alt_association_survives_class_specific_descriptors() {
        let ifaces = interfaces(fixtures::DUALSENSE_DESCRIPTORS);
        // Interface 1 alt 0 is the zero-bandwidth setting every UAC streaming interface has: it
        // carries no endpoints by design, which is exactly why SETINTERFACE to alt 1 is what
        // reserves the isochronous bandwidth.
        let alt0 = ifaces
            .iter()
            .find(|(i, _)| i.number == 1 && i.alt_setting == 0)
            .unwrap();
        assert!(alt0.1.is_empty());
        let alt1 = ifaces
            .iter()
            .find(|(i, _)| i.number == 1 && i.alt_setting == 1)
            .unwrap();
        assert_eq!(alt1.1.len(), 1);
        // The HID interface is still there and still has its own endpoints — the property WP0
        // checks on glass, that claiming the audio interface leaves the gamepad alone.
        let hid = ifaces.iter().find(|(i, _)| i.class == 0x03).unwrap();
        assert_eq!(hid.0.number, 3);
        assert_eq!(hid.1.len(), 2);
    }
}
