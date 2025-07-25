// Licensed to the Apache Software Foundation (ASF) under one
// or more contributor license agreements. See the NOTICE file
// distributed with this work for additional information
// regarding copyright ownership. The ASF licenses this file
// to you under the Apache License, Version 2.0 (the
// "License"); you may not use this file except in compliance
// with the License. You may obtain a copy of the License at
//
//   http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing,
// software distributed under the License is distributed on an
// "AS IS" BASIS, WITHOUT WARRANTIES OR CONDITIONS OF ANY
// KIND, either express or implied. See the License for the
// specific language governing permissions and limitations
// under the License.

use byteorder::{BigEndian, ByteOrder, ReadBytesExt, WriteBytesExt};
use std::convert::{From, TryFrom};

use super::{
    TFieldIdentifier, TInputProtocol, TInputProtocolFactory, TListIdentifier, TMapIdentifier,
    TMessageIdentifier, TMessageType,
};
use super::{TOutputProtocol, TOutputProtocolFactory, TSetIdentifier, TStructIdentifier, TType};
use crate::transport::{TReadTransport, TWriteTransport};
use crate::{ProtocolError, ProtocolErrorKind, TConfiguration};

const BINARY_PROTOCOL_VERSION_1: u32 = 0x8001_0000;

/// Read messages encoded in the Thrift simple binary encoding.
///
/// There are two available modes: `strict` and `non-strict`, where the
/// `non-strict` version does not check for the protocol version in the
/// received message header.
///
/// # Examples
///
/// Create and use a `TBinaryInputProtocol`.
///
/// ```no_run
/// use thrift::protocol::{TBinaryInputProtocol, TInputProtocol};
/// use thrift::transport::TTcpChannel;
///
/// let mut channel = TTcpChannel::new();
/// channel.open("localhost:9090").unwrap();
///
/// let mut protocol = TBinaryInputProtocol::new(channel, true);
///
/// let recvd_bool = protocol.read_bool().unwrap();
/// let recvd_string = protocol.read_string().unwrap();
/// ```
#[derive(Debug)]
pub struct TBinaryInputProtocol<T>
where
    T: TReadTransport,
{
    strict: bool,
    pub transport: T, // FIXME: shouldn't be public
    config: TConfiguration,
    recursion_depth: usize,
}

impl<T> TBinaryInputProtocol<T>
where
    T: TReadTransport,
{
    /// Create a `TBinaryInputProtocol` that reads bytes from `transport`.
    ///
    /// Set `strict` to `true` if all incoming messages contain the protocol
    /// version number in the protocol header.
    pub fn new(transport: T, strict: bool) -> Self {
        Self::with_config(transport, strict, TConfiguration::default())
    }

    pub fn with_config(transport: T, strict: bool, config: TConfiguration) -> Self {
        TBinaryInputProtocol {
            strict,
            transport,
            config,
            recursion_depth: 0,
        }
    }

    fn check_recursion_depth(&self) -> crate::Result<()> {
        if let Some(limit) = self.config.max_recursion_depth() {
            if self.recursion_depth >= limit {
                return Err(crate::Error::Protocol(ProtocolError::new(
                    ProtocolErrorKind::DepthLimit,
                    format!("Maximum recursion depth {} exceeded", limit),
                )));
            }
        }
        Ok(())
    }
}

impl<T> TInputProtocol for TBinaryInputProtocol<T>
where
    T: TReadTransport,
{
    #[allow(clippy::collapsible_if)]
    fn read_message_begin(&mut self) -> crate::Result<TMessageIdentifier> {
        // TODO: Once specialization is stable, call the message size tracking here
        let mut first_bytes = vec![0; 4];
        self.transport.read_exact(&mut first_bytes[..])?;

        // the thrift version header is intentionally negative
        // so the first check we'll do is see if the sign bit is set
        // and if so - assume it's the protocol-version header
        if (first_bytes[0] & 0x80) != 0 {
            // apparently we got a protocol-version header - check
            // it, and if it matches, read the rest of the fields
            if first_bytes[0..2] != [0x80, 0x01] {
                Err(crate::Error::Protocol(ProtocolError {
                    kind: ProtocolErrorKind::BadVersion,
                    message: format!("received bad version: {:?}", &first_bytes[0..2]),
                }))
            } else {
                let message_type: TMessageType = TryFrom::try_from(first_bytes[3])?;
                let name = self.read_string()?;
                let sequence_number = self.read_i32()?;
                Ok(TMessageIdentifier::new(name, message_type, sequence_number))
            }
        } else {
            // apparently we didn't get a protocol-version header,
            // which happens if the sender is not using the strict protocol
            if self.strict {
                // we're in strict mode however, and that always
                // requires the protocol-version header to be written first
                Err(crate::Error::Protocol(ProtocolError {
                    kind: ProtocolErrorKind::BadVersion,
                    message: format!("received bad version: {:?}", &first_bytes[0..2]),
                }))
            } else {
                // in the non-strict version the first message field
                // is the message name. strings (byte arrays) are length-prefixed,
                // so we've just read the length in the first 4 bytes
                let name_size = BigEndian::read_i32(&first_bytes) as usize;
                let mut name_buf: Vec<u8> = vec![0; name_size];
                self.transport.read_exact(&mut name_buf)?;
                let name = String::from_utf8(name_buf)?;

                // read the rest of the fields
                let message_type: TMessageType = self.read_byte().and_then(TryFrom::try_from)?;
                let sequence_number = self.read_i32()?;
                Ok(TMessageIdentifier::new(name, message_type, sequence_number))
            }
        }
    }

    fn read_message_end(&mut self) -> crate::Result<()> {
        Ok(())
    }

    fn read_struct_begin(&mut self) -> crate::Result<Option<TStructIdentifier>> {
        self.check_recursion_depth()?;
        self.recursion_depth += 1;
        Ok(None)
    }

    fn read_struct_end(&mut self) -> crate::Result<()> {
        self.recursion_depth -= 1;
        Ok(())
    }

    fn read_field_begin(&mut self) -> crate::Result<TFieldIdentifier> {
        let field_type_byte = self.read_byte()?;
        let field_type = field_type_from_u8(field_type_byte)?;
        let id = match field_type {
            TType::Stop => Ok(0),
            _ => self.read_i16(),
        }?;
        Ok(TFieldIdentifier::new::<Option<String>, String, i16>(
            None, field_type, id,
        ))
    }

    fn read_field_end(&mut self) -> crate::Result<()> {
        Ok(())
    }

    fn read_bytes(&mut self) -> crate::Result<Vec<u8>> {
        let num_bytes = self.transport.read_i32::<BigEndian>()?;

        if num_bytes < 0 {
            return Err(crate::Error::Protocol(ProtocolError::new(
                ProtocolErrorKind::NegativeSize,
                format!("Negative byte array size: {}", num_bytes),
            )));
        }

        if let Some(max_size) = self.config.max_string_size() {
            if num_bytes as usize > max_size {
                return Err(crate::Error::Protocol(ProtocolError::new(
                    ProtocolErrorKind::SizeLimit,
                    format!(
                        "Byte array size {} exceeds maximum allowed size of {}",
                        num_bytes, max_size
                    ),
                )));
            }
        }

        let mut buf = vec![0u8; num_bytes as usize];
        self.transport
            .read_exact(&mut buf)
            .map(|_| buf)
            .map_err(From::from)
    }

    fn read_bool(&mut self) -> crate::Result<bool> {
        let b = self.read_i8()?;
        match b {
            0 => Ok(false),
            _ => Ok(true),
        }
    }

    fn read_i8(&mut self) -> crate::Result<i8> {
        self.transport.read_i8().map_err(From::from)
    }

    fn read_i16(&mut self) -> crate::Result<i16> {
        self.transport.read_i16::<BigEndian>().map_err(From::from)
    }

    fn read_i32(&mut self) -> crate::Result<i32> {
        self.transport.read_i32::<BigEndian>().map_err(From::from)
    }

    fn read_i64(&mut self) -> crate::Result<i64> {
        self.transport.read_i64::<BigEndian>().map_err(From::from)
    }

    fn read_double(&mut self) -> crate::Result<f64> {
        self.transport.read_f64::<BigEndian>().map_err(From::from)
    }

    fn read_uuid(&mut self) -> crate::Result<uuid::Uuid> {
        let mut buf = [0u8; 16];
        self.transport
            .read_exact(&mut buf)
            .map(|_| uuid::Uuid::from_bytes(buf))
            .map_err(From::from)
    }

    fn read_string(&mut self) -> crate::Result<String> {
        let bytes = self.read_bytes()?;
        String::from_utf8(bytes).map_err(From::from)
    }

    fn read_list_begin(&mut self) -> crate::Result<TListIdentifier> {
        let element_type: TType = self.read_byte().and_then(field_type_from_u8)?;
        let size = self.read_i32()?;
        let min_element_size = self.min_serialized_size(element_type);
        super::check_container_size(&self.config, size, min_element_size)?;
        Ok(TListIdentifier::new(element_type, size))
    }

    fn read_list_end(&mut self) -> crate::Result<()> {
        Ok(())
    }

    fn read_set_begin(&mut self) -> crate::Result<TSetIdentifier> {
        let element_type: TType = self.read_byte().and_then(field_type_from_u8)?;
        let size = self.read_i32()?;
        let min_element_size = self.min_serialized_size(element_type);
        super::check_container_size(&self.config, size, min_element_size)?;
        Ok(TSetIdentifier::new(element_type, size))
    }

    fn read_set_end(&mut self) -> crate::Result<()> {
        Ok(())
    }

    fn read_map_begin(&mut self) -> crate::Result<TMapIdentifier> {
        let key_type: TType = self.read_byte().and_then(field_type_from_u8)?;
        let value_type: TType = self.read_byte().and_then(field_type_from_u8)?;
        let size = self.read_i32()?;

        let key_min_size = self.min_serialized_size(key_type);
        let value_min_size = self.min_serialized_size(value_type);
        let element_size = key_min_size + value_min_size;
        super::check_container_size(&self.config, size, element_size)?;

        Ok(TMapIdentifier::new(key_type, value_type, size))
    }

    fn read_map_end(&mut self) -> crate::Result<()> {
        Ok(())
    }

    // utility
    //

    fn read_byte(&mut self) -> crate::Result<u8> {
        self.transport.read_u8().map_err(From::from)
    }

    fn min_serialized_size(&self, field_type: TType) -> usize {
        match field_type {
            TType::Stop => 1,   // 1 byte minimum
            TType::Void => 1,   // 1 byte minimum
            TType::Bool => 1,   // 1 byte
            TType::I08 => 1,    // 1 byte
            TType::Double => 8, // 8 bytes
            TType::I16 => 2,    // 2 bytes
            TType::I32 => 4,    // 4 bytes
            TType::I64 => 8,    // 8 bytes
            TType::String => 4, // 4 bytes for length prefix
            TType::Struct => 1, // 1 byte minimum (stop field)
            TType::Map => 4,    // 4 bytes size
            TType::Set => 4,    // 4 bytes size
            TType::List => 4,   // 4 bytes size
            TType::Uuid => 16,  // 16 bytes
            TType::Utf7 => 1,   // 1 byte
        }
    }
}

/// Factory for creating instances of `TBinaryInputProtocol`.
#[derive(Default)]
pub struct TBinaryInputProtocolFactory;

impl TBinaryInputProtocolFactory {
    /// Create a `TBinaryInputProtocolFactory`.
    pub fn new() -> TBinaryInputProtocolFactory {
        TBinaryInputProtocolFactory {}
    }
}

impl TInputProtocolFactory for TBinaryInputProtocolFactory {
    fn create(&self, transport: Box<dyn TReadTransport + Send>) -> Box<dyn TInputProtocol + Send> {
        Box::new(TBinaryInputProtocol::new(transport, true))
    }
}

/// Write messages using the Thrift simple binary encoding.
///
/// There are two available modes: `strict` and `non-strict`, where the
/// `strict` version writes the protocol version number in the outgoing message
/// header and the `non-strict` version does not.
///
/// # Examples
///
/// Create and use a `TBinaryOutputProtocol`.
///
/// ```no_run
/// use thrift::protocol::{TBinaryOutputProtocol, TOutputProtocol};
/// use thrift::transport::TTcpChannel;
///
/// let mut channel = TTcpChannel::new();
/// channel.open("localhost:9090").unwrap();
///
/// let mut protocol = TBinaryOutputProtocol::new(channel, true);
///
/// protocol.write_bool(true).unwrap();
/// protocol.write_string("test_string").unwrap();
/// ```
#[derive(Debug)]
pub struct TBinaryOutputProtocol<T>
where
    T: TWriteTransport,
{
    strict: bool,
    pub transport: T, // FIXME: do not make public; only public for testing!
}

impl<T> TBinaryOutputProtocol<T>
where
    T: TWriteTransport,
{
    /// Create a `TBinaryOutputProtocol` that writes bytes to `transport`.
    ///
    /// Set `strict` to `true` if all outgoing messages should contain the
    /// protocol version number in the protocol header.
    pub fn new(transport: T, strict: bool) -> TBinaryOutputProtocol<T> {
        TBinaryOutputProtocol { strict, transport }
    }
}

impl<T> TOutputProtocol for TBinaryOutputProtocol<T>
where
    T: TWriteTransport,
{
    fn write_message_begin(&mut self, identifier: &TMessageIdentifier) -> crate::Result<()> {
        if self.strict {
            let message_type: u8 = identifier.message_type.into();
            let header = BINARY_PROTOCOL_VERSION_1 | (message_type as u32);
            self.transport.write_u32::<BigEndian>(header)?;
            self.write_string(&identifier.name)?;
            self.write_i32(identifier.sequence_number)
        } else {
            self.write_string(&identifier.name)?;
            self.write_byte(identifier.message_type.into())?;
            self.write_i32(identifier.sequence_number)
        }
    }

    fn write_message_end(&mut self) -> crate::Result<()> {
        Ok(())
    }

    fn write_struct_begin(&mut self, _: &TStructIdentifier) -> crate::Result<()> {
        Ok(())
    }

    fn write_struct_end(&mut self) -> crate::Result<()> {
        Ok(())
    }

    fn write_field_begin(&mut self, identifier: &TFieldIdentifier) -> crate::Result<()> {
        if identifier.id.is_none() && identifier.field_type != TType::Stop {
            return Err(crate::Error::Protocol(ProtocolError {
                kind: ProtocolErrorKind::Unknown,
                message: format!(
                    "cannot write identifier {:?} without sequence number",
                    &identifier
                ),
            }));
        }

        self.write_byte(field_type_to_u8(identifier.field_type))?;
        if let Some(id) = identifier.id {
            self.write_i16(id)
        } else {
            Ok(())
        }
    }

    fn write_field_end(&mut self) -> crate::Result<()> {
        Ok(())
    }

    fn write_field_stop(&mut self) -> crate::Result<()> {
        self.write_byte(field_type_to_u8(TType::Stop))
    }

    fn write_bytes(&mut self, b: &[u8]) -> crate::Result<()> {
        self.write_i32(b.len() as i32)?;
        self.transport.write_all(b).map_err(From::from)
    }

    fn write_bool(&mut self, b: bool) -> crate::Result<()> {
        if b {
            self.write_i8(1)
        } else {
            self.write_i8(0)
        }
    }

    fn write_i8(&mut self, i: i8) -> crate::Result<()> {
        self.transport.write_i8(i).map_err(From::from)
    }

    fn write_i16(&mut self, i: i16) -> crate::Result<()> {
        self.transport.write_i16::<BigEndian>(i).map_err(From::from)
    }

    fn write_i32(&mut self, i: i32) -> crate::Result<()> {
        self.transport.write_i32::<BigEndian>(i).map_err(From::from)
    }

    fn write_i64(&mut self, i: i64) -> crate::Result<()> {
        self.transport.write_i64::<BigEndian>(i).map_err(From::from)
    }

    fn write_double(&mut self, d: f64) -> crate::Result<()> {
        self.transport.write_f64::<BigEndian>(d).map_err(From::from)
    }

    fn write_string(&mut self, s: &str) -> crate::Result<()> {
        self.write_bytes(s.as_bytes())
    }

    fn write_uuid(&mut self, uuid: &uuid::Uuid) -> crate::Result<()> {
        self.transport
            .write_all(uuid.as_bytes())
            .map_err(From::from)
    }

    fn write_list_begin(&mut self, identifier: &TListIdentifier) -> crate::Result<()> {
        self.write_byte(field_type_to_u8(identifier.element_type))?;
        self.write_i32(identifier.size)
    }

    fn write_list_end(&mut self) -> crate::Result<()> {
        Ok(())
    }

    fn write_set_begin(&mut self, identifier: &TSetIdentifier) -> crate::Result<()> {
        self.write_byte(field_type_to_u8(identifier.element_type))?;
        self.write_i32(identifier.size)
    }

    fn write_set_end(&mut self) -> crate::Result<()> {
        Ok(())
    }

    fn write_map_begin(&mut self, identifier: &TMapIdentifier) -> crate::Result<()> {
        let key_type = identifier
            .key_type
            .expect("map identifier to write should contain key type");
        self.write_byte(field_type_to_u8(key_type))?;
        let val_type = identifier
            .value_type
            .expect("map identifier to write should contain value type");
        self.write_byte(field_type_to_u8(val_type))?;
        self.write_i32(identifier.size)
    }

    fn write_map_end(&mut self) -> crate::Result<()> {
        Ok(())
    }

    fn flush(&mut self) -> crate::Result<()> {
        self.transport.flush().map_err(From::from)
    }

    // utility
    //

    fn write_byte(&mut self, b: u8) -> crate::Result<()> {
        self.transport.write_u8(b).map_err(From::from)
    }
}

/// Factory for creating instances of `TBinaryOutputProtocol`.
#[derive(Default)]
pub struct TBinaryOutputProtocolFactory;

impl TBinaryOutputProtocolFactory {
    /// Create a `TBinaryOutputProtocolFactory`.
    pub fn new() -> TBinaryOutputProtocolFactory {
        TBinaryOutputProtocolFactory {}
    }
}

impl TOutputProtocolFactory for TBinaryOutputProtocolFactory {
    fn create(
        &self,
        transport: Box<dyn TWriteTransport + Send>,
    ) -> Box<dyn TOutputProtocol + Send> {
        Box::new(TBinaryOutputProtocol::new(transport, true))
    }
}

fn field_type_to_u8(field_type: TType) -> u8 {
    match field_type {
        TType::Stop => 0x00,
        TType::Void => 0x01,
        TType::Bool => 0x02,
        TType::I08 => 0x03, // equivalent to TType::Byte
        TType::Double => 0x04,
        TType::I16 => 0x06,
        TType::I32 => 0x08,
        TType::I64 => 0x0A,
        TType::String | TType::Utf7 => 0x0B,
        TType::Struct => 0x0C,
        TType::Map => 0x0D,
        TType::Set => 0x0E,
        TType::List => 0x0F,
        TType::Uuid => 0x10,
    }
}

fn field_type_from_u8(b: u8) -> crate::Result<TType> {
    match b {
        0x00 => Ok(TType::Stop),
        0x01 => Ok(TType::Void),
        0x02 => Ok(TType::Bool),
        0x03 => Ok(TType::I08), // Equivalent to TType::Byte
        0x04 => Ok(TType::Double),
        0x06 => Ok(TType::I16),
        0x08 => Ok(TType::I32),
        0x0A => Ok(TType::I64),
        0x0B => Ok(TType::String), // technically, also a UTF7, but we'll treat it as string
        0x0C => Ok(TType::Struct),
        0x0D => Ok(TType::Map),
        0x0E => Ok(TType::Set),
        0x0F => Ok(TType::List),
        0x10 => Ok(TType::Uuid),
        unkn => Err(crate::Error::Protocol(ProtocolError {
            kind: ProtocolErrorKind::InvalidData,
            message: format!("cannot convert {} to TType", unkn),
        })),
    }
}

#[cfg(test)]
mod tests {

    use super::*;
    use crate::protocol::{
        TFieldIdentifier, TInputProtocol, TListIdentifier, TMapIdentifier, TMessageIdentifier,
        TMessageType, TOutputProtocol, TSetIdentifier, TStructIdentifier, TType,
    };
    use crate::transport::{ReadHalf, TBufferChannel, TIoChannel, WriteHalf};

    #[test]
    fn must_write_strict_message_call_begin() {
        let (_, mut o_prot) = test_objects(true);

        let ident = TMessageIdentifier::new("test", TMessageType::Call, 1);
        assert!(o_prot.write_message_begin(&ident).is_ok());

        #[rustfmt::skip]
        let expected: [u8; 16] = [
            0x80,
            0x01,
            0x00,
            0x01,
            0x00,
            0x00,
            0x00,
            0x04,
            0x74,
            0x65,
            0x73,
            0x74,
            0x00,
            0x00,
            0x00,
            0x01,
        ];

        assert_eq_written_bytes!(o_prot, expected);
    }

    #[test]
    fn must_write_non_strict_message_call_begin() {
        let (_, mut o_prot) = test_objects(false);

        let ident = TMessageIdentifier::new("test", TMessageType::Call, 1);
        assert!(o_prot.write_message_begin(&ident).is_ok());

        #[rustfmt::skip]
        let expected: [u8; 13] = [
            0x00,
            0x00,
            0x00,
            0x04,
            0x74,
            0x65,
            0x73,
            0x74,
            0x01,
            0x00,
            0x00,
            0x00,
            0x01,
        ];

        assert_eq_written_bytes!(o_prot, expected);
    }

    #[test]
    fn must_write_strict_message_reply_begin() {
        let (_, mut o_prot) = test_objects(true);

        let ident = TMessageIdentifier::new("test", TMessageType::Reply, 10);
        assert!(o_prot.write_message_begin(&ident).is_ok());

        #[rustfmt::skip]
        let expected: [u8; 16] = [
            0x80,
            0x01,
            0x00,
            0x02,
            0x00,
            0x00,
            0x00,
            0x04,
            0x74,
            0x65,
            0x73,
            0x74,
            0x00,
            0x00,
            0x00,
            0x0A,
        ];

        assert_eq_written_bytes!(o_prot, expected);
    }

    #[test]
    fn must_write_non_strict_message_reply_begin() {
        let (_, mut o_prot) = test_objects(false);

        let ident = TMessageIdentifier::new("test", TMessageType::Reply, 10);
        assert!(o_prot.write_message_begin(&ident).is_ok());

        #[rustfmt::skip]
        let expected: [u8; 13] = [
            0x00,
            0x00,
            0x00,
            0x04,
            0x74,
            0x65,
            0x73,
            0x74,
            0x02,
            0x00,
            0x00,
            0x00,
            0x0A,
        ];

        assert_eq_written_bytes!(o_prot, expected);
    }

    #[test]
    fn must_round_trip_strict_message_begin() {
        let (mut i_prot, mut o_prot) = test_objects(true);

        let sent_ident = TMessageIdentifier::new("test", TMessageType::Call, 1);
        assert!(o_prot.write_message_begin(&sent_ident).is_ok());

        copy_write_buffer_to_read_buffer!(o_prot);

        let received_ident = assert_success!(i_prot.read_message_begin());
        assert_eq!(&received_ident, &sent_ident);
    }

    #[test]
    fn must_round_trip_non_strict_message_begin() {
        let (mut i_prot, mut o_prot) = test_objects(false);

        let sent_ident = TMessageIdentifier::new("test", TMessageType::Call, 1);
        assert!(o_prot.write_message_begin(&sent_ident).is_ok());

        copy_write_buffer_to_read_buffer!(o_prot);

        let received_ident = assert_success!(i_prot.read_message_begin());
        assert_eq!(&received_ident, &sent_ident);
    }

    #[test]
    fn must_write_message_end() {
        assert_no_write(|o| o.write_message_end(), true);
    }

    #[test]
    fn must_write_struct_begin() {
        assert_no_write(
            |o| o.write_struct_begin(&TStructIdentifier::new("foo")),
            true,
        );
    }

    #[test]
    fn must_write_struct_end() {
        assert_no_write(|o| o.write_struct_end(), true);
    }

    #[test]
    fn must_write_field_begin() {
        let (_, mut o_prot) = test_objects(true);

        assert!(o_prot
            .write_field_begin(&TFieldIdentifier::new("some_field", TType::String, 22))
            .is_ok());

        let expected: [u8; 3] = [0x0B, 0x00, 0x16];
        assert_eq_written_bytes!(o_prot, expected);
    }

    #[test]
    fn must_round_trip_field_begin() {
        let (mut i_prot, mut o_prot) = test_objects(true);

        let sent_field_ident = TFieldIdentifier::new("foo", TType::I64, 20);
        assert!(o_prot.write_field_begin(&sent_field_ident).is_ok());

        copy_write_buffer_to_read_buffer!(o_prot);

        let expected_ident = TFieldIdentifier {
            name: None,
            field_type: TType::I64,
            id: Some(20),
        }; // no name
        let received_ident = assert_success!(i_prot.read_field_begin());
        assert_eq!(&received_ident, &expected_ident);
    }

    #[test]
    fn must_write_stop_field() {
        let (_, mut o_prot) = test_objects(true);

        assert!(o_prot.write_field_stop().is_ok());

        let expected: [u8; 1] = [0x00];
        assert_eq_written_bytes!(o_prot, expected);
    }

    #[test]
    fn must_round_trip_field_stop() {
        let (mut i_prot, mut o_prot) = test_objects(true);

        assert!(o_prot.write_field_stop().is_ok());

        copy_write_buffer_to_read_buffer!(o_prot);

        let expected_ident = TFieldIdentifier {
            name: None,
            field_type: TType::Stop,
            id: Some(0),
        }; // we get id 0

        let received_ident = assert_success!(i_prot.read_field_begin());
        assert_eq!(&received_ident, &expected_ident);
    }

    #[test]
    fn must_write_field_end() {
        assert_no_write(|o| o.write_field_end(), true);
    }

    #[test]
    fn must_write_list_begin() {
        let (_, mut o_prot) = test_objects(true);

        assert!(o_prot
            .write_list_begin(&TListIdentifier::new(TType::Bool, 5))
            .is_ok());

        let expected: [u8; 5] = [0x02, 0x00, 0x00, 0x00, 0x05];
        assert_eq_written_bytes!(o_prot, expected);
    }

    #[test]
    fn must_round_trip_list_begin() {
        let (mut i_prot, mut o_prot) = test_objects(true);

        let ident = TListIdentifier::new(TType::I32, 4);
        assert!(o_prot.write_list_begin(&ident).is_ok());
        assert!(o_prot.write_i32(10).is_ok());
        assert!(o_prot.write_i32(20).is_ok());
        assert!(o_prot.write_i32(30).is_ok());
        assert!(o_prot.write_i32(40).is_ok());

        assert!(o_prot.write_list_end().is_ok());

        copy_write_buffer_to_read_buffer!(o_prot);

        let received_ident = assert_success!(i_prot.read_list_begin());
        assert_eq!(&received_ident, &ident);

        assert_eq!(i_prot.read_i32().unwrap(), 10);
        assert_eq!(i_prot.read_i32().unwrap(), 20);
        assert_eq!(i_prot.read_i32().unwrap(), 30);
        assert_eq!(i_prot.read_i32().unwrap(), 40);

        assert!(i_prot.read_list_end().is_ok());
    }

    #[test]
    fn must_write_list_end() {
        assert_no_write(|o| o.write_list_end(), true);
    }

    #[test]
    fn must_write_set_begin() {
        let (_, mut o_prot) = test_objects(true);

        assert!(o_prot
            .write_set_begin(&TSetIdentifier::new(TType::I16, 7))
            .is_ok());

        let expected: [u8; 5] = [0x06, 0x00, 0x00, 0x00, 0x07];
        assert_eq_written_bytes!(o_prot, expected);
    }

    #[test]
    fn must_round_trip_set_begin() {
        let (mut i_prot, mut o_prot) = test_objects(true);

        let ident = TSetIdentifier::new(TType::I64, 3);
        assert!(o_prot.write_set_begin(&ident).is_ok());
        assert!(o_prot.write_i64(123).is_ok());
        assert!(o_prot.write_i64(456).is_ok());
        assert!(o_prot.write_i64(789).is_ok());

        assert!(o_prot.write_set_end().is_ok());

        copy_write_buffer_to_read_buffer!(o_prot);

        let received_ident_result = i_prot.read_set_begin();
        assert!(received_ident_result.is_ok());
        assert_eq!(&received_ident_result.unwrap(), &ident);

        assert_eq!(i_prot.read_i64().unwrap(), 123);
        assert_eq!(i_prot.read_i64().unwrap(), 456);
        assert_eq!(i_prot.read_i64().unwrap(), 789);

        assert!(i_prot.read_set_end().is_ok());
    }

    #[test]
    fn must_write_set_end() {
        assert_no_write(|o| o.write_set_end(), true);
    }

    #[test]
    fn must_write_map_begin() {
        let (_, mut o_prot) = test_objects(true);

        assert!(o_prot
            .write_map_begin(&TMapIdentifier::new(TType::I64, TType::Struct, 32))
            .is_ok());

        let expected: [u8; 6] = [0x0A, 0x0C, 0x00, 0x00, 0x00, 0x20];
        assert_eq_written_bytes!(o_prot, expected);
    }

    #[test]
    fn must_round_trip_map_begin() {
        let (mut i_prot, mut o_prot) = test_objects(true);

        let ident = TMapIdentifier::new(TType::String, TType::I32, 2);
        assert!(o_prot.write_map_begin(&ident).is_ok());
        assert!(o_prot.write_string("key1").is_ok());
        assert!(o_prot.write_i32(100).is_ok());
        assert!(o_prot.write_string("key2").is_ok());
        assert!(o_prot.write_i32(200).is_ok());

        assert!(o_prot.write_map_end().is_ok());

        copy_write_buffer_to_read_buffer!(o_prot);

        let received_ident = assert_success!(i_prot.read_map_begin());
        assert_eq!(&received_ident, &ident);

        assert_eq!(i_prot.read_string().unwrap(), "key1");
        assert_eq!(i_prot.read_i32().unwrap(), 100);
        assert_eq!(i_prot.read_string().unwrap(), "key2");
        assert_eq!(i_prot.read_i32().unwrap(), 200);

        assert!(i_prot.read_map_end().is_ok());
    }

    #[test]
    fn must_write_map_end() {
        assert_no_write(|o| o.write_map_end(), true);
    }

    #[test]
    fn must_write_bool_true() {
        let (_, mut o_prot) = test_objects(true);

        assert!(o_prot.write_bool(true).is_ok());

        let expected: [u8; 1] = [0x01];
        assert_eq_written_bytes!(o_prot, expected);
    }

    #[test]
    fn must_write_bool_false() {
        let (_, mut o_prot) = test_objects(true);

        assert!(o_prot.write_bool(false).is_ok());

        let expected: [u8; 1] = [0x00];
        assert_eq_written_bytes!(o_prot, expected);
    }

    #[test]
    fn must_read_bool_true() {
        let (mut i_prot, _) = test_objects(true);

        set_readable_bytes!(i_prot, &[0x01]);

        let read_bool = assert_success!(i_prot.read_bool());
        assert!(read_bool);
    }

    #[test]
    fn must_read_bool_false() {
        let (mut i_prot, _) = test_objects(true);

        set_readable_bytes!(i_prot, &[0x00]);

        let read_bool = assert_success!(i_prot.read_bool());
        assert!(!read_bool);
    }

    #[test]
    fn must_allow_any_non_zero_value_to_be_interpreted_as_bool_true() {
        let (mut i_prot, _) = test_objects(true);

        set_readable_bytes!(i_prot, &[0xAC]);

        let read_bool = assert_success!(i_prot.read_bool());
        assert!(read_bool);
    }

    #[test]
    fn must_write_bytes() {
        let (_, mut o_prot) = test_objects(true);

        let bytes: [u8; 10] = [0x0A, 0xCC, 0xD1, 0x84, 0x99, 0x12, 0xAB, 0xBB, 0x45, 0xDF];

        assert!(o_prot.write_bytes(&bytes).is_ok());

        let buf = o_prot.transport.write_bytes();
        assert_eq!(&buf[0..4], [0x00, 0x00, 0x00, 0x0A]); // length
        assert_eq!(&buf[4..], bytes); // actual bytes
    }

    #[test]
    fn must_write_uuid() {
        let (_, mut o_prot) = test_objects(true);
        let uuid = uuid::Uuid::new_v4();
        assert!(o_prot.write_uuid(&uuid).is_ok());
        let buf = o_prot.transport.write_bytes();
        assert_eq!(&buf, uuid.as_bytes());
    }

    #[test]
    fn must_round_trip_uuid() {
        let (mut i_prot, mut o_prot) = test_objects(true);
        let uuid = uuid::uuid!("F9168C5E-CEB2-4faa-B6BF-329BF39FA1E4");
        assert!(o_prot.write_uuid(&uuid).is_ok());
        copy_write_buffer_to_read_buffer!(o_prot);
        let received_uuid = assert_success!(i_prot.read_uuid());
        assert_eq!(&received_uuid, &uuid);
    }

    #[test]
    fn must_round_trip_bytes() {
        let (mut i_prot, mut o_prot) = test_objects(true);

        #[rustfmt::skip]
        let bytes: [u8; 25] = [
            0x20,
            0xFD,
            0x18,
            0x84,
            0x99,
            0x12,
            0xAB,
            0xBB,
            0x45,
            0xDF,
            0x34,
            0xDC,
            0x98,
            0xA4,
            0x6D,
            0xF3,
            0x99,
            0xB4,
            0xB7,
            0xD4,
            0x9C,
            0xA5,
            0xB3,
            0xC9,
            0x88,
        ];

        assert!(o_prot.write_bytes(&bytes).is_ok());

        copy_write_buffer_to_read_buffer!(o_prot);

        let received_bytes = assert_success!(i_prot.read_bytes());
        assert_eq!(&received_bytes, &bytes);
    }

    fn test_objects(
        strict: bool,
    ) -> (
        TBinaryInputProtocol<ReadHalf<TBufferChannel>>,
        TBinaryOutputProtocol<WriteHalf<TBufferChannel>>,
    ) {
        let mem = TBufferChannel::with_capacity(200, 200);

        let (r_mem, w_mem) = mem.split().unwrap();

        let i_prot = TBinaryInputProtocol::new(r_mem, strict);
        let o_prot = TBinaryOutputProtocol::new(w_mem, strict);

        (i_prot, o_prot)
    }

    fn assert_no_write<F>(mut write_fn: F, strict: bool)
    where
        F: FnMut(&mut TBinaryOutputProtocol<WriteHalf<TBufferChannel>>) -> crate::Result<()>,
    {
        let (_, mut o_prot) = test_objects(strict);
        assert!(write_fn(&mut o_prot).is_ok());
        assert_eq!(o_prot.transport.write_bytes().len(), 0);
    }

    #[test]
    fn must_enforce_recursion_depth_limit() {
        let mem = TBufferChannel::with_capacity(40, 40);
        let (r_mem, _) = mem.split().unwrap();

        let config = TConfiguration::builder()
            .max_recursion_depth(Some(2))
            .build()
            .unwrap();
        let mut i_prot = TBinaryInputProtocol::with_config(r_mem, true, config);

        assert!(i_prot.read_struct_begin().is_ok());
        assert_eq!(i_prot.recursion_depth, 1);

        assert!(i_prot.read_struct_begin().is_ok());
        assert_eq!(i_prot.recursion_depth, 2);

        let result = i_prot.read_struct_begin();
        assert!(result.is_err());
        match result {
            Err(crate::Error::Protocol(e)) => {
                assert_eq!(e.kind, ProtocolErrorKind::DepthLimit);
            }
            _ => panic!("Expected protocol error with DepthLimit"),
        }

        assert!(i_prot.read_struct_end().is_ok());
        assert_eq!(i_prot.recursion_depth, 1);
        assert!(i_prot.read_struct_end().is_ok());
        assert_eq!(i_prot.recursion_depth, 0);
    }

    #[test]
    fn must_reject_negative_container_sizes() {
        let mem = TBufferChannel::with_capacity(40, 40);
        let (r_mem, mut w_mem) = mem.split().unwrap();

        let mut i_prot = TBinaryInputProtocol::new(r_mem, true);

        w_mem.set_readable_bytes(&[0x0F, 0xFF, 0xFF, 0xFF, 0xFF]);

        let result = i_prot.read_list_begin();
        assert!(result.is_err());
        match result {
            Err(crate::Error::Protocol(e)) => {
                assert_eq!(e.kind, ProtocolErrorKind::NegativeSize);
            }
            _ => panic!("Expected protocol error with NegativeSize"),
        }
    }

    #[test]
    fn must_enforce_container_size_limit() {
        let mem = TBufferChannel::with_capacity(40, 40);
        let (r_mem, mut w_mem) = mem.split().unwrap();

        let config = TConfiguration::builder()
            .max_container_size(Some(100))
            .build()
            .unwrap();

        let mut i_prot = TBinaryInputProtocol::with_config(r_mem, true, config);

        w_mem.set_readable_bytes(&[0x0F, 0x00, 0x00, 0x00, 0xC8]);

        let result = i_prot.read_list_begin();
        assert!(result.is_err());
        match result {
            Err(crate::Error::Protocol(e)) => {
                assert_eq!(e.kind, ProtocolErrorKind::SizeLimit);
                assert!(e
                    .message
                    .contains("Container size 200 exceeds maximum allowed size of 100"));
            }
            _ => panic!("Expected protocol error with SizeLimit"),
        }
    }

    #[test]
    fn must_allow_containers_within_limit() {
        let mem = TBufferChannel::with_capacity(200, 200);
        let (r_mem, mut w_mem) = mem.split().unwrap();

        // Create protocol with container limit of 100
        let config = TConfiguration::builder()
            .max_container_size(Some(100))
            .build()
            .unwrap();
        let mut i_prot = TBinaryInputProtocol::with_config(r_mem, true, config);

        let mut data = vec![0x08]; // TType::I32
        data.extend_from_slice(&5i32.to_be_bytes()); // size = 5

        for i in 1i32..=5i32 {
            data.extend_from_slice(&(i * 10).to_be_bytes());
        }

        w_mem.set_readable_bytes(&data);

        let result = i_prot.read_list_begin();
        assert!(result.is_ok());
        let list_ident = result.unwrap();
        assert_eq!(list_ident.size, 5);
        assert_eq!(list_ident.element_type, TType::I32);
    }

    #[test]
    fn must_enforce_string_size_limit() {
        let mem = TBufferChannel::with_capacity(100, 100);
        let (r_mem, mut w_mem) = mem.split().unwrap();

        let config = TConfiguration::builder()
            .max_string_size(Some(1000))
            .build()
            .unwrap();
        let mut i_prot = TBinaryInputProtocol::with_config(r_mem, true, config);

        w_mem.set_readable_bytes(&[0x00, 0x00, 0x07, 0xD0]);

        let result = i_prot.read_string();
        assert!(result.is_err());
        match result {
            Err(crate::Error::Protocol(e)) => {
                assert_eq!(e.kind, ProtocolErrorKind::SizeLimit);
                assert!(e
                    .message
                    .contains("Byte array size 2000 exceeds maximum allowed size of 1000"));
            }
            _ => panic!("Expected protocol error with SizeLimit"),
        }
    }

    #[test]
    fn must_allow_strings_within_limit() {
        let mem = TBufferChannel::with_capacity(100, 100);
        let (r_mem, mut w_mem) = mem.split().unwrap();

        let config = TConfiguration::builder()
            .max_string_size(Some(1000))
            .build()
            .unwrap();
        let mut i_prot = TBinaryInputProtocol::with_config(r_mem, true, config);

        w_mem.set_readable_bytes(&[0x00, 0x00, 0x00, 0x05, b'h', b'e', b'l', b'l', b'o']);

        let result = i_prot.read_string();
        assert!(result.is_ok());
        assert_eq!(result.unwrap(), "hello");
    }
}
