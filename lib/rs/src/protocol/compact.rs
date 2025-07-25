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

use byteorder::{LittleEndian, ReadBytesExt, WriteBytesExt};
use integer_encoding::{VarIntReader, VarIntWriter};
use std::convert::{From, TryFrom};
use std::io;

use super::{
    TFieldIdentifier, TInputProtocol, TInputProtocolFactory, TListIdentifier, TMapIdentifier,
    TMessageIdentifier, TMessageType,
};
use super::{TOutputProtocol, TOutputProtocolFactory, TSetIdentifier, TStructIdentifier, TType};
use crate::transport::{TReadTransport, TWriteTransport};
use crate::{ProtocolError, ProtocolErrorKind, TConfiguration};

const COMPACT_PROTOCOL_ID: u8 = 0x82;
const COMPACT_VERSION: u8 = 0x01;
const COMPACT_VERSION_MASK: u8 = 0x1F;

/// Read messages encoded in the Thrift compact protocol.
///
/// # Examples
///
/// Create and use a `TCompactInputProtocol`.
///
/// ```no_run
/// use thrift::protocol::{TCompactInputProtocol, TInputProtocol};
/// use thrift::transport::TTcpChannel;
///
/// let mut channel = TTcpChannel::new();
/// channel.open("localhost:9090").unwrap();
///
/// let mut protocol = TCompactInputProtocol::new(channel);
///
/// let recvd_bool = protocol.read_bool().unwrap();
/// let recvd_string = protocol.read_string().unwrap();
/// ```
#[derive(Debug)]
pub struct TCompactInputProtocol<T>
where
    T: TReadTransport,
{
    // Identifier of the last field deserialized for a struct.
    last_read_field_id: i16,
    // Stack of the last read field ids (a new entry is added each time a nested struct is read).
    read_field_id_stack: Vec<i16>,
    // Boolean value for a field.
    // Saved because boolean fields and their value are encoded in a single byte,
    // and reading the field only occurs after the field id is read.
    pending_read_bool_value: Option<bool>,
    // Underlying transport used for byte-level operations.
    transport: T,
    // Configuration
    config: TConfiguration,
    // Current recursion depth
    recursion_depth: usize,
}

impl<T> TCompactInputProtocol<T>
where
    T: TReadTransport,
{
    /// Create a `TCompactInputProtocol` that reads bytes from `transport`.
    pub fn new(transport: T) -> TCompactInputProtocol<T> {
        Self::with_config(transport, TConfiguration::default())
    }

    /// Create a `TCompactInputProtocol` with custom configuration.
    pub fn with_config(transport: T, config: TConfiguration) -> TCompactInputProtocol<T> {
        TCompactInputProtocol {
            last_read_field_id: 0,
            read_field_id_stack: Vec::new(),
            pending_read_bool_value: None,
            transport,
            config,
            recursion_depth: 0,
        }
    }

    fn read_list_set_begin(&mut self) -> crate::Result<(TType, i32)> {
        let header = self.read_byte()?;
        let element_type = collection_u8_to_type(header & 0x0F)?;

        let possible_element_count = (header & 0xF0) >> 4;
        let element_count = if possible_element_count != 15 {
            // high bits set high if count and type encoded separately
            possible_element_count as i32
        } else {
            self.transport.read_varint::<u32>()? as i32
        };

        let min_element_size = self.min_serialized_size(element_type);
        super::check_container_size(&self.config, element_count, min_element_size)?;

        Ok((element_type, element_count))
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

impl<T> TInputProtocol for TCompactInputProtocol<T>
where
    T: TReadTransport,
{
    fn read_message_begin(&mut self) -> crate::Result<TMessageIdentifier> {
        // TODO: Once specialization is stable, call the message size tracking here
        let compact_id = self.read_byte()?;
        if compact_id != COMPACT_PROTOCOL_ID {
            Err(crate::Error::Protocol(crate::ProtocolError {
                kind: crate::ProtocolErrorKind::BadVersion,
                message: format!("invalid compact protocol header {:?}", compact_id),
            }))
        } else {
            Ok(())
        }?;

        let type_and_byte = self.read_byte()?;
        let received_version = type_and_byte & COMPACT_VERSION_MASK;
        if received_version != COMPACT_VERSION {
            Err(crate::Error::Protocol(crate::ProtocolError {
                kind: crate::ProtocolErrorKind::BadVersion,
                message: format!(
                    "cannot process compact protocol version {:?}",
                    received_version
                ),
            }))
        } else {
            Ok(())
        }?;

        // NOTE: unsigned right shift will pad with 0s
        let message_type: TMessageType = TMessageType::try_from(type_and_byte >> 5)?;
        // writing side wrote signed sequence number as u32 to avoid zigzag encoding
        let sequence_number = self.transport.read_varint::<u32>()? as i32;
        let service_call_name = self.read_string()?;

        self.last_read_field_id = 0;

        Ok(TMessageIdentifier::new(
            service_call_name,
            message_type,
            sequence_number,
        ))
    }

    fn read_message_end(&mut self) -> crate::Result<()> {
        Ok(())
    }

    fn read_struct_begin(&mut self) -> crate::Result<Option<TStructIdentifier>> {
        self.check_recursion_depth()?;
        self.recursion_depth += 1;
        self.read_field_id_stack.push(self.last_read_field_id);
        self.last_read_field_id = 0;
        Ok(None)
    }

    fn read_struct_end(&mut self) -> crate::Result<()> {
        self.recursion_depth -= 1;
        self.last_read_field_id = self
            .read_field_id_stack
            .pop()
            .expect("should have previous field ids");
        Ok(())
    }

    fn read_field_begin(&mut self) -> crate::Result<TFieldIdentifier> {
        // we can read at least one byte, which is:
        // - the type
        // - the field delta and the type
        let field_type = self.read_byte()?;
        let field_delta = (field_type & 0xF0) >> 4;
        let field_type = match field_type & 0x0F {
            0x01 => {
                self.pending_read_bool_value = Some(true);
                Ok(TType::Bool)
            }
            0x02 => {
                self.pending_read_bool_value = Some(false);
                Ok(TType::Bool)
            }
            ttu8 => u8_to_type(ttu8),
        }?;

        match field_type {
            TType::Stop => Ok(
                TFieldIdentifier::new::<Option<String>, String, Option<i16>>(
                    None,
                    TType::Stop,
                    None,
                ),
            ),
            _ => {
                if field_delta != 0 {
                    self.last_read_field_id += field_delta as i16;
                } else {
                    self.last_read_field_id = self.read_i16()?;
                };

                Ok(TFieldIdentifier {
                    name: None,
                    field_type,
                    id: Some(self.last_read_field_id),
                })
            }
        }
    }

    fn read_field_end(&mut self) -> crate::Result<()> {
        Ok(())
    }

    fn read_bool(&mut self) -> crate::Result<bool> {
        match self.pending_read_bool_value.take() {
            Some(b) => Ok(b),
            None => {
                let b = self.read_byte()?;
                match b {
                    // Previous versions of the thrift compact protocol specification said to use 0
                    // and 1 inside collections, but that differed from existing implementations.
                    // The specification was updated in https://github.com/apache/thrift/commit/2c29c5665bc442e703480bb0ee60fe925ffe02e8.
                    0x00 => Ok(false),
                    0x01 => Ok(true),
                    0x02 => Ok(false),
                    unkn => Err(crate::Error::Protocol(crate::ProtocolError {
                        kind: crate::ProtocolErrorKind::InvalidData,
                        message: format!("cannot convert {} into bool", unkn),
                    })),
                }
            }
        }
    }

    fn read_bytes(&mut self) -> crate::Result<Vec<u8>> {
        let len = self.transport.read_varint::<u32>()?;

        if let Some(max_size) = self.config.max_string_size() {
            if len as usize > max_size {
                return Err(crate::Error::Protocol(ProtocolError::new(
                    ProtocolErrorKind::SizeLimit,
                    format!(
                        "Byte array size {} exceeds maximum allowed size of {}",
                        len, max_size
                    ),
                )));
            }
        }

        let mut buf = vec![0u8; len as usize];
        self.transport
            .read_exact(&mut buf)
            .map_err(From::from)
            .map(|_| buf)
    }

    fn read_i8(&mut self) -> crate::Result<i8> {
        self.read_byte().map(|i| i as i8)
    }

    fn read_i16(&mut self) -> crate::Result<i16> {
        self.transport.read_varint::<i16>().map_err(From::from)
    }

    fn read_i32(&mut self) -> crate::Result<i32> {
        self.transport.read_varint::<i32>().map_err(From::from)
    }

    fn read_i64(&mut self) -> crate::Result<i64> {
        self.transport.read_varint::<i64>().map_err(From::from)
    }

    fn read_double(&mut self) -> crate::Result<f64> {
        self.transport
            .read_f64::<LittleEndian>()
            .map_err(From::from)
    }

    fn read_uuid(&mut self) -> crate::Result<uuid::Uuid> {
        uuid::Uuid::from_slice(&self.read_bytes()?).map_err(From::from)
    }

    fn read_string(&mut self) -> crate::Result<String> {
        let bytes = self.read_bytes()?;
        String::from_utf8(bytes).map_err(From::from)
    }

    fn read_list_begin(&mut self) -> crate::Result<TListIdentifier> {
        let (element_type, element_count) = self.read_list_set_begin()?;
        Ok(TListIdentifier::new(element_type, element_count))
    }

    fn read_list_end(&mut self) -> crate::Result<()> {
        Ok(())
    }

    fn read_set_begin(&mut self) -> crate::Result<TSetIdentifier> {
        let (element_type, element_count) = self.read_list_set_begin()?;
        Ok(TSetIdentifier::new(element_type, element_count))
    }

    fn read_set_end(&mut self) -> crate::Result<()> {
        Ok(())
    }

    fn read_map_begin(&mut self) -> crate::Result<TMapIdentifier> {
        let element_count = self.transport.read_varint::<u32>()? as i32;
        if element_count == 0 {
            Ok(TMapIdentifier::new(None, None, 0))
        } else {
            let type_header = self.read_byte()?;
            let key_type = collection_u8_to_type((type_header & 0xF0) >> 4)?;
            let val_type = collection_u8_to_type(type_header & 0x0F)?;

            let key_min_size = self.min_serialized_size(key_type);
            let value_min_size = self.min_serialized_size(val_type);
            let element_size = key_min_size + value_min_size;
            super::check_container_size(&self.config, element_count, element_size)?;

            Ok(TMapIdentifier::new(key_type, val_type, element_count))
        }
    }

    fn read_map_end(&mut self) -> crate::Result<()> {
        Ok(())
    }

    // utility
    //

    fn read_byte(&mut self) -> crate::Result<u8> {
        let mut buf = [0u8; 1];
        self.transport
            .read_exact(&mut buf)
            .map_err(From::from)
            .map(|_| buf[0])
    }

    fn min_serialized_size(&self, field_type: TType) -> usize {
        compact_protocol_min_serialized_size(field_type)
    }
}

pub(crate) fn compact_protocol_min_serialized_size(field_type: TType) -> usize {
    match field_type {
        TType::Stop => 1,   // 1 byte
        TType::Void => 1,   // 1 byte
        TType::Bool => 1,   // 1 byte
        TType::I08 => 1,    // 1 byte
        TType::Double => 8, // 8 bytes (not varint encoded)
        TType::I16 => 1,    // 1 byte minimum (varint)
        TType::I32 => 1,    // 1 byte minimum (varint)
        TType::I64 => 1,    // 1 byte minimum (varint)
        TType::String => 1, // 1 byte minimum for length (varint)
        TType::Struct => 1, // 1 byte minimum (stop field)
        TType::Map => 1,    // 1 byte minimum
        TType::Set => 1,    // 1 byte minimum
        TType::List => 1,   // 1 byte minimum
        TType::Uuid => 16,  // 16 bytes
        TType::Utf7 => 1,   // 1 byte
    }
}

impl<T> io::Seek for TCompactInputProtocol<T>
where
    T: io::Seek + TReadTransport,
{
    fn seek(&mut self, pos: io::SeekFrom) -> io::Result<u64> {
        self.transport.seek(pos)
    }
}

/// Factory for creating instances of `TCompactInputProtocol`.
#[derive(Default)]
pub struct TCompactInputProtocolFactory;

impl TCompactInputProtocolFactory {
    /// Create a `TCompactInputProtocolFactory`.
    pub fn new() -> TCompactInputProtocolFactory {
        TCompactInputProtocolFactory {}
    }
}

impl TInputProtocolFactory for TCompactInputProtocolFactory {
    fn create(&self, transport: Box<dyn TReadTransport + Send>) -> Box<dyn TInputProtocol + Send> {
        Box::new(TCompactInputProtocol::new(transport))
    }
}

/// Write messages using the Thrift compact protocol.
///
/// # Examples
///
/// Create and use a `TCompactOutputProtocol`.
///
/// ```no_run
/// use thrift::protocol::{TCompactOutputProtocol, TOutputProtocol};
/// use thrift::transport::TTcpChannel;
///
/// let mut channel = TTcpChannel::new();
/// channel.open("localhost:9090").unwrap();
///
/// let mut protocol = TCompactOutputProtocol::new(channel);
///
/// protocol.write_bool(true).unwrap();
/// protocol.write_string("test_string").unwrap();
/// ```
#[derive(Debug)]
pub struct TCompactOutputProtocol<T>
where
    T: TWriteTransport,
{
    // Identifier of the last field serialized for a struct.
    last_write_field_id: i16,
    // Stack of the last written field ids (new entry added each time a nested struct is written).
    write_field_id_stack: Vec<i16>,
    // Field identifier of the boolean field to be written.
    // Saved because boolean fields and their value are encoded in a single byte
    pending_write_bool_field_identifier: Option<TFieldIdentifier>,
    // Underlying transport used for byte-level operations.
    transport: T,
}

impl<T> TCompactOutputProtocol<T>
where
    T: TWriteTransport,
{
    /// Create a `TCompactOutputProtocol` that writes bytes to `transport`.
    pub fn new(transport: T) -> TCompactOutputProtocol<T> {
        TCompactOutputProtocol {
            last_write_field_id: 0,
            write_field_id_stack: Vec::new(),
            pending_write_bool_field_identifier: None,
            transport,
        }
    }

    // FIXME: field_type as unconstrained u8 is bad
    fn write_field_header(&mut self, field_type: u8, field_id: i16) -> crate::Result<()> {
        let field_delta = field_id - self.last_write_field_id;
        if field_delta > 0 && field_delta < 15 {
            self.write_byte(((field_delta as u8) << 4) | field_type)?;
        } else {
            self.write_byte(field_type)?;
            self.write_i16(field_id)?;
        }
        self.last_write_field_id = field_id;
        Ok(())
    }

    fn write_list_set_begin(
        &mut self,
        element_type: TType,
        element_count: i32,
    ) -> crate::Result<()> {
        let elem_identifier = collection_type_to_u8(element_type);
        if element_count <= 14 {
            let header = (element_count as u8) << 4 | elem_identifier;
            self.write_byte(header)
        } else {
            let header = 0xF0 | elem_identifier;
            self.write_byte(header)?;
            // element count is strictly positive as per the spec, so
            // cast i32 as u32 so that varint writing won't use zigzag encoding
            self.transport
                .write_varint(element_count as u32)
                .map_err(From::from)
                .map(|_| ())
        }
    }

    fn assert_no_pending_bool_write(&self) {
        if let Some(ref f) = self.pending_write_bool_field_identifier {
            panic!("pending bool field {:?} not written", f)
        }
    }
}

impl<T> TOutputProtocol for TCompactOutputProtocol<T>
where
    T: TWriteTransport,
{
    fn write_message_begin(&mut self, identifier: &TMessageIdentifier) -> crate::Result<()> {
        self.write_byte(COMPACT_PROTOCOL_ID)?;
        self.write_byte((u8::from(identifier.message_type) << 5) | COMPACT_VERSION)?;
        // cast i32 as u32 so that varint writing won't use zigzag encoding
        self.transport
            .write_varint(identifier.sequence_number as u32)?;
        self.write_string(&identifier.name)?;
        Ok(())
    }

    fn write_message_end(&mut self) -> crate::Result<()> {
        self.assert_no_pending_bool_write();
        Ok(())
    }

    fn write_struct_begin(&mut self, _: &TStructIdentifier) -> crate::Result<()> {
        self.write_field_id_stack.push(self.last_write_field_id);
        self.last_write_field_id = 0;
        Ok(())
    }

    fn write_struct_end(&mut self) -> crate::Result<()> {
        self.assert_no_pending_bool_write();
        self.last_write_field_id = self
            .write_field_id_stack
            .pop()
            .expect("should have previous field ids");
        Ok(())
    }

    fn write_field_begin(&mut self, identifier: &TFieldIdentifier) -> crate::Result<()> {
        match identifier.field_type {
            TType::Bool => {
                if self.pending_write_bool_field_identifier.is_some() {
                    panic!(
                        "should not have a pending bool while writing another bool with id: \
                         {:?}",
                        identifier
                    )
                }
                self.pending_write_bool_field_identifier = Some(identifier.clone());
                Ok(())
            }
            _ => {
                let field_type = type_to_u8(identifier.field_type);
                let field_id = identifier.id.expect("non-stop field should have field id");
                self.write_field_header(field_type, field_id)
            }
        }
    }

    fn write_field_end(&mut self) -> crate::Result<()> {
        self.assert_no_pending_bool_write();
        Ok(())
    }

    fn write_field_stop(&mut self) -> crate::Result<()> {
        self.assert_no_pending_bool_write();
        self.write_byte(type_to_u8(TType::Stop))
    }

    fn write_bool(&mut self, b: bool) -> crate::Result<()> {
        match self.pending_write_bool_field_identifier.take() {
            Some(pending) => {
                let field_id = pending.id.expect("bool field should have a field id");
                let field_type_as_u8 = if b { 0x01 } else { 0x02 };
                self.write_field_header(field_type_as_u8, field_id)
            }
            None => {
                if b {
                    self.write_byte(0x01)
                } else {
                    self.write_byte(0x02)
                }
            }
        }
    }

    fn write_bytes(&mut self, b: &[u8]) -> crate::Result<()> {
        // length is strictly positive as per the spec, so
        // cast i32 as u32 so that varint writing won't use zigzag encoding
        self.transport.write_varint(b.len() as u32)?;
        self.transport.write_all(b).map_err(From::from)
    }

    fn write_i8(&mut self, i: i8) -> crate::Result<()> {
        self.write_byte(i as u8)
    }

    fn write_i16(&mut self, i: i16) -> crate::Result<()> {
        self.transport
            .write_varint(i)
            .map_err(From::from)
            .map(|_| ())
    }

    fn write_i32(&mut self, i: i32) -> crate::Result<()> {
        self.transport
            .write_varint(i)
            .map_err(From::from)
            .map(|_| ())
    }

    fn write_i64(&mut self, i: i64) -> crate::Result<()> {
        self.transport
            .write_varint(i)
            .map_err(From::from)
            .map(|_| ())
    }

    fn write_double(&mut self, d: f64) -> crate::Result<()> {
        self.transport
            .write_f64::<LittleEndian>(d)
            .map_err(From::from)
    }

    fn write_uuid(&mut self, uuid: &uuid::Uuid) -> crate::Result<()> {
        self.write_bytes(uuid.as_bytes())
    }

    fn write_string(&mut self, s: &str) -> crate::Result<()> {
        self.write_bytes(s.as_bytes())
    }

    fn write_list_begin(&mut self, identifier: &TListIdentifier) -> crate::Result<()> {
        self.write_list_set_begin(identifier.element_type, identifier.size)
    }

    fn write_list_end(&mut self) -> crate::Result<()> {
        Ok(())
    }

    fn write_set_begin(&mut self, identifier: &TSetIdentifier) -> crate::Result<()> {
        self.write_list_set_begin(identifier.element_type, identifier.size)
    }

    fn write_set_end(&mut self) -> crate::Result<()> {
        Ok(())
    }

    fn write_map_begin(&mut self, identifier: &TMapIdentifier) -> crate::Result<()> {
        if identifier.size == 0 {
            self.write_byte(0)
        } else {
            // element count is strictly positive as per the spec, so
            // cast i32 as u32 so that varint writing won't use zigzag encoding
            self.transport.write_varint(identifier.size as u32)?;

            let key_type = identifier
                .key_type
                .expect("map identifier to write should contain key type");
            let key_type_byte = collection_type_to_u8(key_type) << 4;

            let val_type = identifier
                .value_type
                .expect("map identifier to write should contain value type");
            let val_type_byte = collection_type_to_u8(val_type);

            let map_type_header = key_type_byte | val_type_byte;
            self.write_byte(map_type_header)
        }
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
        self.transport.write(&[b]).map_err(From::from).map(|_| ())
    }
}

/// Factory for creating instances of `TCompactOutputProtocol`.
#[derive(Default)]
pub struct TCompactOutputProtocolFactory;

impl TCompactOutputProtocolFactory {
    /// Create a `TCompactOutputProtocolFactory`.
    pub fn new() -> TCompactOutputProtocolFactory {
        TCompactOutputProtocolFactory {}
    }
}

impl TOutputProtocolFactory for TCompactOutputProtocolFactory {
    fn create(
        &self,
        transport: Box<dyn TWriteTransport + Send>,
    ) -> Box<dyn TOutputProtocol + Send> {
        Box::new(TCompactOutputProtocol::new(transport))
    }
}

fn collection_type_to_u8(field_type: TType) -> u8 {
    match field_type {
        TType::Bool => 0x01,
        f => type_to_u8(f),
    }
}

fn type_to_u8(field_type: TType) -> u8 {
    match field_type {
        TType::Stop => 0x00,
        TType::I08 => 0x03, // equivalent to TType::Byte
        TType::I16 => 0x04,
        TType::I32 => 0x05,
        TType::I64 => 0x06,
        TType::Double => 0x07,
        TType::String => 0x08,
        TType::List => 0x09,
        TType::Set => 0x0A,
        TType::Map => 0x0B,
        TType::Struct => 0x0C,
        TType::Uuid => 0x0D,
        _ => panic!("should not have attempted to convert {} to u8", field_type),
    }
}

fn collection_u8_to_type(b: u8) -> crate::Result<TType> {
    match b {
        // For historical and compatibility reasons, a reader should be capable to deal with both cases.
        // The only valid value in the original spec was 2, but due to a widespread implementation bug
        // the defacto standard across large parts of the library became 1 instead.
        // As a result, both values are now allowed.
        0x01 | 0x02 => Ok(TType::Bool),
        o => u8_to_type(o),
    }
}

fn u8_to_type(b: u8) -> crate::Result<TType> {
    match b {
        0x00 => Ok(TType::Stop),
        0x03 => Ok(TType::I08), // equivalent to TType::Byte
        0x04 => Ok(TType::I16),
        0x05 => Ok(TType::I32),
        0x06 => Ok(TType::I64),
        0x07 => Ok(TType::Double),
        0x08 => Ok(TType::String),
        0x09 => Ok(TType::List),
        0x0A => Ok(TType::Set),
        0x0B => Ok(TType::Map),
        0x0C => Ok(TType::Struct),
        0x0D => Ok(TType::Uuid),
        unkn => Err(crate::Error::Protocol(crate::ProtocolError {
            kind: crate::ProtocolErrorKind::InvalidData,
            message: format!("cannot convert {} into TType", unkn),
        })),
    }
}

#[cfg(test)]
mod tests {

    use crate::protocol::{
        TFieldIdentifier, TInputProtocol, TListIdentifier, TMapIdentifier, TMessageIdentifier,
        TMessageType, TOutputProtocol, TSetIdentifier, TStructIdentifier, TType,
    };
    use crate::transport::{ReadHalf, TBufferChannel, TIoChannel, WriteHalf};

    use super::*;

    #[test]
    fn must_write_message_begin_largest_maximum_positive_sequence_number() {
        let (_, mut o_prot) = test_objects();

        assert_success!(o_prot.write_message_begin(&TMessageIdentifier::new(
            "bar",
            TMessageType::Reply,
            i32::MAX
        )));

        #[rustfmt::skip]
        let expected: [u8; 11] = [
            0x82, /* protocol ID */
            0x41, /* message type | protocol version */
            0xFF,
            0xFF,
            0xFF,
            0xFF,
            0x07, /* non-zig-zag varint sequence number */
            0x03, /* message-name length */
            0x62,
            0x61,
            0x72 /* "bar" */,
        ];

        assert_eq_written_bytes!(o_prot, expected);
    }

    #[test]
    fn must_read_message_begin_largest_maximum_positive_sequence_number() {
        let (mut i_prot, _) = test_objects();

        #[rustfmt::skip]
        let source_bytes: [u8; 11] = [
            0x82, /* protocol ID */
            0x41, /* message type | protocol version */
            0xFF,
            0xFF,
            0xFF,
            0xFF,
            0x07, /* non-zig-zag varint sequence number */
            0x03, /* message-name length */
            0x62,
            0x61,
            0x72 /* "bar" */,
        ];

        i_prot.transport.set_readable_bytes(&source_bytes);

        let expected = TMessageIdentifier::new("bar", TMessageType::Reply, i32::MAX);
        let res = assert_success!(i_prot.read_message_begin());

        assert_eq!(&expected, &res);
    }

    #[test]
    fn must_write_message_begin_positive_sequence_number_0() {
        let (_, mut o_prot) = test_objects();

        assert_success!(o_prot.write_message_begin(&TMessageIdentifier::new(
            "foo",
            TMessageType::Call,
            431
        )));

        #[rustfmt::skip]
        let expected: [u8; 8] = [
            0x82, /* protocol ID */
            0x21, /* message type | protocol version */
            0xAF,
            0x03, /* non-zig-zag varint sequence number */
            0x03, /* message-name length */
            0x66,
            0x6F,
            0x6F /* "foo" */,
        ];

        assert_eq_written_bytes!(o_prot, expected);
    }

    #[test]
    fn must_read_message_begin_positive_sequence_number_0() {
        let (mut i_prot, _) = test_objects();

        #[rustfmt::skip]
        let source_bytes: [u8; 8] = [
            0x82, /* protocol ID */
            0x21, /* message type | protocol version */
            0xAF,
            0x03, /* non-zig-zag varint sequence number */
            0x03, /* message-name length */
            0x66,
            0x6F,
            0x6F /* "foo" */,
        ];

        i_prot.transport.set_readable_bytes(&source_bytes);

        let expected = TMessageIdentifier::new("foo", TMessageType::Call, 431);
        let res = assert_success!(i_prot.read_message_begin());

        assert_eq!(&expected, &res);
    }

    #[test]
    fn must_write_message_begin_positive_sequence_number_1() {
        let (_, mut o_prot) = test_objects();

        assert_success!(o_prot.write_message_begin(&TMessageIdentifier::new(
            "bar",
            TMessageType::Reply,
            991_828
        )));

        #[rustfmt::skip]
        let expected: [u8; 9] = [
            0x82, /* protocol ID */
            0x41, /* message type | protocol version */
            0xD4,
            0xC4,
            0x3C, /* non-zig-zag varint sequence number */
            0x03, /* message-name length */
            0x62,
            0x61,
            0x72 /* "bar" */,
        ];

        assert_eq_written_bytes!(o_prot, expected);
    }

    #[test]
    fn must_read_message_begin_positive_sequence_number_1() {
        let (mut i_prot, _) = test_objects();

        #[rustfmt::skip]
        let source_bytes: [u8; 9] = [
            0x82, /* protocol ID */
            0x41, /* message type | protocol version */
            0xD4,
            0xC4,
            0x3C, /* non-zig-zag varint sequence number */
            0x03, /* message-name length */
            0x62,
            0x61,
            0x72 /* "bar" */,
        ];

        i_prot.transport.set_readable_bytes(&source_bytes);

        let expected = TMessageIdentifier::new("bar", TMessageType::Reply, 991_828);
        let res = assert_success!(i_prot.read_message_begin());

        assert_eq!(&expected, &res);
    }

    #[test]
    fn must_write_message_begin_zero_sequence_number() {
        let (_, mut o_prot) = test_objects();

        assert_success!(o_prot.write_message_begin(&TMessageIdentifier::new(
            "bar",
            TMessageType::Reply,
            0
        )));

        #[rustfmt::skip]
        let expected: [u8; 7] = [
            0x82, /* protocol ID */
            0x41, /* message type | protocol version */
            0x00, /* non-zig-zag varint sequence number */
            0x03, /* message-name length */
            0x62,
            0x61,
            0x72 /* "bar" */,
        ];

        assert_eq_written_bytes!(o_prot, expected);
    }

    #[test]
    fn must_read_message_begin_zero_sequence_number() {
        let (mut i_prot, _) = test_objects();

        #[rustfmt::skip]
        let source_bytes: [u8; 7] = [
            0x82, /* protocol ID */
            0x41, /* message type | protocol version */
            0x00, /* non-zig-zag varint sequence number */
            0x03, /* message-name length */
            0x62,
            0x61,
            0x72 /* "bar" */,
        ];

        i_prot.transport.set_readable_bytes(&source_bytes);

        let expected = TMessageIdentifier::new("bar", TMessageType::Reply, 0);
        let res = assert_success!(i_prot.read_message_begin());

        assert_eq!(&expected, &res);
    }

    #[test]
    fn must_write_message_begin_largest_minimum_negative_sequence_number() {
        let (_, mut o_prot) = test_objects();

        assert_success!(o_prot.write_message_begin(&TMessageIdentifier::new(
            "bar",
            TMessageType::Reply,
            i32::MIN
        )));

        // two's complement notation of i32::MIN = 1000_0000_0000_0000_0000_0000_0000_0000
        #[rustfmt::skip]
        let expected: [u8; 11] = [
            0x82, /* protocol ID */
            0x41, /* message type | protocol version */
            0x80,
            0x80,
            0x80,
            0x80,
            0x08, /* non-zig-zag varint sequence number */
            0x03, /* message-name length */
            0x62,
            0x61,
            0x72 /* "bar" */,
        ];

        assert_eq_written_bytes!(o_prot, expected);
    }

    #[test]
    fn must_read_message_begin_largest_minimum_negative_sequence_number() {
        let (mut i_prot, _) = test_objects();

        // two's complement notation of i32::MIN = 1000_0000_0000_0000_0000_0000_0000_0000
        #[rustfmt::skip]
        let source_bytes: [u8; 11] = [
            0x82, /* protocol ID */
            0x41, /* message type | protocol version */
            0x80,
            0x80,
            0x80,
            0x80,
            0x08, /* non-zig-zag varint sequence number */
            0x03, /* message-name length */
            0x62,
            0x61,
            0x72 /* "bar" */,
        ];

        i_prot.transport.set_readable_bytes(&source_bytes);

        let expected = TMessageIdentifier::new("bar", TMessageType::Reply, i32::MIN);
        let res = assert_success!(i_prot.read_message_begin());

        assert_eq!(&expected, &res);
    }

    #[test]
    fn must_write_message_begin_negative_sequence_number_0() {
        let (_, mut o_prot) = test_objects();

        assert_success!(o_prot.write_message_begin(&TMessageIdentifier::new(
            "foo",
            TMessageType::Call,
            -431
        )));

        // signed two's complement of -431 = 1111_1111_1111_1111_1111_1110_0101_0001
        #[rustfmt::skip]
        let expected: [u8; 11] = [
            0x82, /* protocol ID */
            0x21, /* message type | protocol version */
            0xD1,
            0xFC,
            0xFF,
            0xFF,
            0x0F, /* non-zig-zag varint sequence number */
            0x03, /* message-name length */
            0x66,
            0x6F,
            0x6F /* "foo" */,
        ];

        assert_eq_written_bytes!(o_prot, expected);
    }

    #[test]
    fn must_read_message_begin_negative_sequence_number_0() {
        let (mut i_prot, _) = test_objects();

        // signed two's complement of -431 = 1111_1111_1111_1111_1111_1110_0101_0001
        #[rustfmt::skip]
        let source_bytes: [u8; 11] = [
            0x82, /* protocol ID */
            0x21, /* message type | protocol version */
            0xD1,
            0xFC,
            0xFF,
            0xFF,
            0x0F, /* non-zig-zag varint sequence number */
            0x03, /* message-name length */
            0x66,
            0x6F,
            0x6F /* "foo" */,
        ];

        i_prot.transport.set_readable_bytes(&source_bytes);

        let expected = TMessageIdentifier::new("foo", TMessageType::Call, -431);
        let res = assert_success!(i_prot.read_message_begin());

        assert_eq!(&expected, &res);
    }

    #[test]
    fn must_write_message_begin_negative_sequence_number_1() {
        let (_, mut o_prot) = test_objects();

        assert_success!(o_prot.write_message_begin(&TMessageIdentifier::new(
            "foo",
            TMessageType::Call,
            -73_184_125
        )));

        // signed two's complement of -73184125 = 1111_1011_1010_0011_0100_1100_1000_0011
        #[rustfmt::skip]
        let expected: [u8; 11] = [
            0x82, /* protocol ID */
            0x21, /* message type | protocol version */
            0x83,
            0x99,
            0x8D,
            0xDD,
            0x0F, /* non-zig-zag varint sequence number */
            0x03, /* message-name length */
            0x66,
            0x6F,
            0x6F /* "foo" */,
        ];

        assert_eq_written_bytes!(o_prot, expected);
    }

    #[test]
    fn must_read_message_begin_negative_sequence_number_1() {
        let (mut i_prot, _) = test_objects();

        // signed two's complement of -73184125 = 1111_1011_1010_0011_0100_1100_1000_0011
        #[rustfmt::skip]
        let source_bytes: [u8; 11] = [
            0x82, /* protocol ID */
            0x21, /* message type | protocol version */
            0x83,
            0x99,
            0x8D,
            0xDD,
            0x0F, /* non-zig-zag varint sequence number */
            0x03, /* message-name length */
            0x66,
            0x6F,
            0x6F /* "foo" */,
        ];

        i_prot.transport.set_readable_bytes(&source_bytes);

        let expected = TMessageIdentifier::new("foo", TMessageType::Call, -73_184_125);
        let res = assert_success!(i_prot.read_message_begin());

        assert_eq!(&expected, &res);
    }

    #[test]
    fn must_write_message_begin_negative_sequence_number_2() {
        let (_, mut o_prot) = test_objects();

        assert_success!(o_prot.write_message_begin(&TMessageIdentifier::new(
            "foo",
            TMessageType::Call,
            -1_073_741_823
        )));

        // signed two's complement of -1073741823 = 1100_0000_0000_0000_0000_0000_0000_0001
        #[rustfmt::skip]
        let expected: [u8; 11] = [
            0x82, /* protocol ID */
            0x21, /* message type | protocol version */
            0x81,
            0x80,
            0x80,
            0x80,
            0x0C, /* non-zig-zag varint sequence number */
            0x03, /* message-name length */
            0x66,
            0x6F,
            0x6F /* "foo" */,
        ];

        assert_eq_written_bytes!(o_prot, expected);
    }

    #[test]
    fn must_read_message_begin_negative_sequence_number_2() {
        let (mut i_prot, _) = test_objects();

        // signed two's complement of -1073741823 = 1100_0000_0000_0000_0000_0000_0000_0001
        #[rustfmt::skip]
        let source_bytes: [u8; 11] = [
            0x82, /* protocol ID */
            0x21, /* message type | protocol version */
            0x81,
            0x80,
            0x80,
            0x80,
            0x0C, /* non-zig-zag varint sequence number */
            0x03, /* message-name length */
            0x66,
            0x6F,
            0x6F, /* "foo" */
        ];

        i_prot.transport.set_readable_bytes(&source_bytes);

        let expected = TMessageIdentifier::new("foo", TMessageType::Call, -1_073_741_823);
        let res = assert_success!(i_prot.read_message_begin());

        assert_eq!(&expected, &res);
    }

    #[test]
    fn must_round_trip_upto_i64_maxvalue() {
        // See https://issues.apache.org/jira/browse/THRIFT-5131
        for i in 0..64 {
            let (mut i_prot, mut o_prot) = test_objects();
            let val: i64 = ((1u64 << i) - 1) as i64;

            o_prot
                .write_field_begin(&TFieldIdentifier::new("val", TType::I64, 1))
                .unwrap();
            o_prot.write_i64(val).unwrap();
            o_prot.write_field_end().unwrap();
            o_prot.flush().unwrap();

            copy_write_buffer_to_read_buffer!(o_prot);

            i_prot.read_field_begin().unwrap();
            assert_eq!(val, i_prot.read_i64().unwrap());
        }
    }

    #[test]
    fn must_round_trip_message_begin() {
        let (mut i_prot, mut o_prot) = test_objects();

        let ident = TMessageIdentifier::new("service_call", TMessageType::Call, 1_283_948);

        assert_success!(o_prot.write_message_begin(&ident));

        copy_write_buffer_to_read_buffer!(o_prot);

        let res = assert_success!(i_prot.read_message_begin());
        assert_eq!(&res, &ident);
    }

    #[test]
    fn must_write_message_end() {
        assert_no_write(|o| o.write_message_end());
    }

    // NOTE: structs and fields are tested together
    //

    #[test]
    fn must_write_struct_with_delta_fields() {
        let (_, mut o_prot) = test_objects();

        // no bytes should be written however
        assert_success!(o_prot.write_struct_begin(&TStructIdentifier::new("foo")));

        // write three fields with tiny field ids
        // since they're small the field ids will be encoded as deltas

        // since this is the first field (and it's zero) it gets the full varint write
        assert_success!(o_prot.write_field_begin(&TFieldIdentifier::new("foo", TType::I08, 0)));
        assert_success!(o_prot.write_field_end());

        // since this delta > 0 and < 15 it can be encoded as a delta
        assert_success!(o_prot.write_field_begin(&TFieldIdentifier::new("foo", TType::I16, 4)));
        assert_success!(o_prot.write_field_end());

        // since this delta > 0 and < 15 it can be encoded as a delta
        assert_success!(o_prot.write_field_begin(&TFieldIdentifier::new("foo", TType::List, 9)));
        assert_success!(o_prot.write_field_end());

        // now, finish the struct off
        assert_success!(o_prot.write_field_stop());
        assert_success!(o_prot.write_struct_end());

        #[rustfmt::skip]
        let expected: [u8; 5] = [
            0x03, /* field type */
            0x00, /* first field id */
            0x44, /* field delta (4) | field type */
            0x59, /* field delta (5) | field type */
            0x00 /* field stop */,
        ];

        assert_eq_written_bytes!(o_prot, expected);
    }

    #[test]
    fn must_round_trip_struct_with_delta_fields() {
        let (mut i_prot, mut o_prot) = test_objects();

        // no bytes should be written however
        assert_success!(o_prot.write_struct_begin(&TStructIdentifier::new("foo")));

        // write three fields with tiny field ids
        // since they're small the field ids will be encoded as deltas

        // since this is the first field (and it's zero) it gets the full varint write
        let field_ident_1 = TFieldIdentifier::new("foo", TType::I08, 0);
        assert_success!(o_prot.write_field_begin(&field_ident_1));
        assert_success!(o_prot.write_field_end());

        // since this delta > 0 and < 15 it can be encoded as a delta
        let field_ident_2 = TFieldIdentifier::new("foo", TType::I16, 4);
        assert_success!(o_prot.write_field_begin(&field_ident_2));
        assert_success!(o_prot.write_field_end());

        // since this delta > 0 and < 15 it can be encoded as a delta
        let field_ident_3 = TFieldIdentifier::new("foo", TType::List, 9);
        assert_success!(o_prot.write_field_begin(&field_ident_3));
        assert_success!(o_prot.write_field_end());

        // now, finish the struct off
        assert_success!(o_prot.write_field_stop());
        assert_success!(o_prot.write_struct_end());

        copy_write_buffer_to_read_buffer!(o_prot);

        // read the struct back
        assert_success!(i_prot.read_struct_begin());

        let read_ident_1 = assert_success!(i_prot.read_field_begin());
        assert_eq!(
            read_ident_1,
            TFieldIdentifier {
                name: None,
                ..field_ident_1
            }
        );
        assert_success!(i_prot.read_field_end());

        let read_ident_2 = assert_success!(i_prot.read_field_begin());
        assert_eq!(
            read_ident_2,
            TFieldIdentifier {
                name: None,
                ..field_ident_2
            }
        );
        assert_success!(i_prot.read_field_end());

        let read_ident_3 = assert_success!(i_prot.read_field_begin());
        assert_eq!(
            read_ident_3,
            TFieldIdentifier {
                name: None,
                ..field_ident_3
            }
        );
        assert_success!(i_prot.read_field_end());

        let read_ident_4 = assert_success!(i_prot.read_field_begin());
        assert_eq!(
            read_ident_4,
            TFieldIdentifier {
                name: None,
                field_type: TType::Stop,
                id: None,
            }
        );

        assert_success!(i_prot.read_struct_end());
    }

    #[test]
    fn must_write_struct_with_non_zero_initial_field_and_delta_fields() {
        let (_, mut o_prot) = test_objects();

        // no bytes should be written however
        assert_success!(o_prot.write_struct_begin(&TStructIdentifier::new("foo")));

        // write three fields with tiny field ids
        // since they're small the field ids will be encoded as deltas

        // gets a delta write
        assert_success!(o_prot.write_field_begin(&TFieldIdentifier::new("foo", TType::I32, 1)));
        assert_success!(o_prot.write_field_end());

        // since this delta > 0 and < 15 it can be encoded as a delta
        assert_success!(o_prot.write_field_begin(&TFieldIdentifier::new("foo", TType::Set, 2)));
        assert_success!(o_prot.write_field_end());

        // since this delta > 0 and < 15 it can be encoded as a delta
        assert_success!(o_prot.write_field_begin(&TFieldIdentifier::new("foo", TType::String, 6)));
        assert_success!(o_prot.write_field_end());

        // now, finish the struct off
        assert_success!(o_prot.write_field_stop());
        assert_success!(o_prot.write_struct_end());

        #[rustfmt::skip]
        let expected: [u8; 4] = [
            0x15, /* field delta (1) | field type */
            0x1A, /* field delta (1) | field type */
            0x48, /* field delta (4) | field type */
            0x00 /* field stop */,
        ];

        assert_eq_written_bytes!(o_prot, expected);
    }

    #[test]
    fn must_round_trip_struct_with_non_zero_initial_field_and_delta_fields() {
        let (mut i_prot, mut o_prot) = test_objects();

        // no bytes should be written however
        assert_success!(o_prot.write_struct_begin(&TStructIdentifier::new("foo")));

        // write three fields with tiny field ids
        // since they're small the field ids will be encoded as deltas

        // gets a delta write
        let field_ident_1 = TFieldIdentifier::new("foo", TType::I32, 1);
        assert_success!(o_prot.write_field_begin(&field_ident_1));
        assert_success!(o_prot.write_field_end());

        // since this delta > 0 and < 15 it can be encoded as a delta
        let field_ident_2 = TFieldIdentifier::new("foo", TType::Set, 2);
        assert_success!(o_prot.write_field_begin(&field_ident_2));
        assert_success!(o_prot.write_field_end());

        // since this delta > 0 and < 15 it can be encoded as a delta
        let field_ident_3 = TFieldIdentifier::new("foo", TType::String, 6);
        assert_success!(o_prot.write_field_begin(&field_ident_3));
        assert_success!(o_prot.write_field_end());

        // now, finish the struct off
        assert_success!(o_prot.write_field_stop());
        assert_success!(o_prot.write_struct_end());

        copy_write_buffer_to_read_buffer!(o_prot);

        // read the struct back
        assert_success!(i_prot.read_struct_begin());

        let read_ident_1 = assert_success!(i_prot.read_field_begin());
        assert_eq!(
            read_ident_1,
            TFieldIdentifier {
                name: None,
                ..field_ident_1
            }
        );
        assert_success!(i_prot.read_field_end());

        let read_ident_2 = assert_success!(i_prot.read_field_begin());
        assert_eq!(
            read_ident_2,
            TFieldIdentifier {
                name: None,
                ..field_ident_2
            }
        );
        assert_success!(i_prot.read_field_end());

        let read_ident_3 = assert_success!(i_prot.read_field_begin());
        assert_eq!(
            read_ident_3,
            TFieldIdentifier {
                name: None,
                ..field_ident_3
            }
        );
        assert_success!(i_prot.read_field_end());

        let read_ident_4 = assert_success!(i_prot.read_field_begin());
        assert_eq!(
            read_ident_4,
            TFieldIdentifier {
                name: None,
                field_type: TType::Stop,
                id: None,
            }
        );

        assert_success!(i_prot.read_struct_end());
    }

    #[test]
    fn must_write_struct_with_long_fields() {
        let (_, mut o_prot) = test_objects();

        // no bytes should be written however
        assert_success!(o_prot.write_struct_begin(&TStructIdentifier::new("foo")));

        // write three fields with field ids that cannot be encoded as deltas

        // since this is the first field (and it's zero) it gets the full varint write
        assert_success!(o_prot.write_field_begin(&TFieldIdentifier::new("foo", TType::I32, 0)));
        assert_success!(o_prot.write_field_end());

        // since this delta is > 15 it is encoded as a zig-zag varint
        assert_success!(o_prot.write_field_begin(&TFieldIdentifier::new("foo", TType::I64, 16)));
        assert_success!(o_prot.write_field_end());

        // since this delta is > 15 it is encoded as a zig-zag varint
        assert_success!(o_prot.write_field_begin(&TFieldIdentifier::new("foo", TType::Set, 99)));
        assert_success!(o_prot.write_field_end());

        // now, finish the struct off
        assert_success!(o_prot.write_field_stop());
        assert_success!(o_prot.write_struct_end());

        #[rustfmt::skip]
        let expected: [u8; 8] = [
            0x05, /* field type */
            0x00, /* first field id */
            0x06, /* field type */
            0x20, /* zig-zag varint field id */
            0x0A, /* field type */
            0xC6,
            0x01, /* zig-zag varint field id */
            0x00 /* field stop */,
        ];

        assert_eq_written_bytes!(o_prot, expected);
    }

    #[test]
    fn must_round_trip_struct_with_long_fields() {
        let (mut i_prot, mut o_prot) = test_objects();

        // no bytes should be written however
        assert_success!(o_prot.write_struct_begin(&TStructIdentifier::new("foo")));

        // write three fields with field ids that cannot be encoded as deltas

        // since this is the first field (and it's zero) it gets the full varint write
        let field_ident_1 = TFieldIdentifier::new("foo", TType::I32, 0);
        assert_success!(o_prot.write_field_begin(&field_ident_1));
        assert_success!(o_prot.write_field_end());

        // since this delta is > 15 it is encoded as a zig-zag varint
        let field_ident_2 = TFieldIdentifier::new("foo", TType::I64, 16);
        assert_success!(o_prot.write_field_begin(&field_ident_2));
        assert_success!(o_prot.write_field_end());

        // since this delta is > 15 it is encoded as a zig-zag varint
        let field_ident_3 = TFieldIdentifier::new("foo", TType::Set, 99);
        assert_success!(o_prot.write_field_begin(&field_ident_3));
        assert_success!(o_prot.write_field_end());

        // now, finish the struct off
        assert_success!(o_prot.write_field_stop());
        assert_success!(o_prot.write_struct_end());

        copy_write_buffer_to_read_buffer!(o_prot);

        // read the struct back
        assert_success!(i_prot.read_struct_begin());

        let read_ident_1 = assert_success!(i_prot.read_field_begin());
        assert_eq!(
            read_ident_1,
            TFieldIdentifier {
                name: None,
                ..field_ident_1
            }
        );
        assert_success!(i_prot.read_field_end());

        let read_ident_2 = assert_success!(i_prot.read_field_begin());
        assert_eq!(
            read_ident_2,
            TFieldIdentifier {
                name: None,
                ..field_ident_2
            }
        );
        assert_success!(i_prot.read_field_end());

        let read_ident_3 = assert_success!(i_prot.read_field_begin());
        assert_eq!(
            read_ident_3,
            TFieldIdentifier {
                name: None,
                ..field_ident_3
            }
        );
        assert_success!(i_prot.read_field_end());

        let read_ident_4 = assert_success!(i_prot.read_field_begin());
        assert_eq!(
            read_ident_4,
            TFieldIdentifier {
                name: None,
                field_type: TType::Stop,
                id: None,
            }
        );

        assert_success!(i_prot.read_struct_end());
    }

    #[test]
    fn must_write_struct_with_mix_of_long_and_delta_fields() {
        let (_, mut o_prot) = test_objects();

        // no bytes should be written however
        assert_success!(o_prot.write_struct_begin(&TStructIdentifier::new("foo")));

        // write three fields with field ids that cannot be encoded as deltas

        // since the delta is > 0 and < 15 it gets a delta write
        assert_success!(o_prot.write_field_begin(&TFieldIdentifier::new("foo", TType::I64, 1)));
        assert_success!(o_prot.write_field_end());

        // since this delta > 0 and < 15 it gets a delta write
        assert_success!(o_prot.write_field_begin(&TFieldIdentifier::new("foo", TType::I32, 9)));
        assert_success!(o_prot.write_field_end());

        // since this delta is > 15 it is encoded as a zig-zag varint
        assert_success!(o_prot.write_field_begin(&TFieldIdentifier::new("foo", TType::Set, 1000)));
        assert_success!(o_prot.write_field_end());

        // since this delta is > 15 it is encoded as a zig-zag varint
        assert_success!(o_prot.write_field_begin(&TFieldIdentifier::new("foo", TType::Set, 2001)));
        assert_success!(o_prot.write_field_end());

        // since this is only 3 up from the previous it is recorded as a delta
        assert_success!(o_prot.write_field_begin(&TFieldIdentifier::new("foo", TType::Set, 2004)));
        assert_success!(o_prot.write_field_end());

        // now, finish the struct off
        assert_success!(o_prot.write_field_stop());
        assert_success!(o_prot.write_struct_end());

        #[rustfmt::skip]
        let expected: [u8; 10] = [
            0x16, /* field delta (1) | field type */
            0x85, /* field delta (8) | field type */
            0x0A, /* field type */
            0xD0,
            0x0F, /* zig-zag varint field id */
            0x0A, /* field type */
            0xA2,
            0x1F, /* zig-zag varint field id */
            0x3A, /* field delta (3) | field type */
            0x00 /* field stop */,
        ];

        assert_eq_written_bytes!(o_prot, expected);
    }

    #[allow(clippy::cognitive_complexity)]
    #[test]
    fn must_round_trip_struct_with_mix_of_long_and_delta_fields() {
        let (mut i_prot, mut o_prot) = test_objects();

        // no bytes should be written however
        let struct_ident = TStructIdentifier::new("foo");
        assert_success!(o_prot.write_struct_begin(&struct_ident));

        // write three fields with field ids that cannot be encoded as deltas

        // since the delta is > 0 and < 15 it gets a delta write
        let field_ident_1 = TFieldIdentifier::new("foo", TType::I64, 1);
        assert_success!(o_prot.write_field_begin(&field_ident_1));
        assert_success!(o_prot.write_field_end());

        // since this delta > 0 and < 15 it gets a delta write
        let field_ident_2 = TFieldIdentifier::new("foo", TType::I32, 9);
        assert_success!(o_prot.write_field_begin(&field_ident_2));
        assert_success!(o_prot.write_field_end());

        // since this delta is > 15 it is encoded as a zig-zag varint
        let field_ident_3 = TFieldIdentifier::new("foo", TType::Set, 1000);
        assert_success!(o_prot.write_field_begin(&field_ident_3));
        assert_success!(o_prot.write_field_end());

        // since this delta is > 15 it is encoded as a zig-zag varint
        let field_ident_4 = TFieldIdentifier::new("foo", TType::Set, 2001);
        assert_success!(o_prot.write_field_begin(&field_ident_4));
        assert_success!(o_prot.write_field_end());

        // since this is only 3 up from the previous it is recorded as a delta
        let field_ident_5 = TFieldIdentifier::new("foo", TType::Set, 2004);
        assert_success!(o_prot.write_field_begin(&field_ident_5));
        assert_success!(o_prot.write_field_end());

        // now, finish the struct off
        assert_success!(o_prot.write_field_stop());
        assert_success!(o_prot.write_struct_end());

        copy_write_buffer_to_read_buffer!(o_prot);

        // read the struct back
        assert_success!(i_prot.read_struct_begin());

        let read_ident_1 = assert_success!(i_prot.read_field_begin());
        assert_eq!(
            read_ident_1,
            TFieldIdentifier {
                name: None,
                ..field_ident_1
            }
        );
        assert_success!(i_prot.read_field_end());

        let read_ident_2 = assert_success!(i_prot.read_field_begin());
        assert_eq!(
            read_ident_2,
            TFieldIdentifier {
                name: None,
                ..field_ident_2
            }
        );
        assert_success!(i_prot.read_field_end());

        let read_ident_3 = assert_success!(i_prot.read_field_begin());
        assert_eq!(
            read_ident_3,
            TFieldIdentifier {
                name: None,
                ..field_ident_3
            }
        );
        assert_success!(i_prot.read_field_end());

        let read_ident_4 = assert_success!(i_prot.read_field_begin());
        assert_eq!(
            read_ident_4,
            TFieldIdentifier {
                name: None,
                ..field_ident_4
            }
        );
        assert_success!(i_prot.read_field_end());

        let read_ident_5 = assert_success!(i_prot.read_field_begin());
        assert_eq!(
            read_ident_5,
            TFieldIdentifier {
                name: None,
                ..field_ident_5
            }
        );
        assert_success!(i_prot.read_field_end());

        let read_ident_6 = assert_success!(i_prot.read_field_begin());
        assert_eq!(
            read_ident_6,
            TFieldIdentifier {
                name: None,
                field_type: TType::Stop,
                id: None,
            }
        );

        assert_success!(i_prot.read_struct_end());
    }

    #[test]
    fn must_write_nested_structs_0() {
        // last field of the containing struct is a delta
        // first field of the the contained struct is a delta

        let (_, mut o_prot) = test_objects();

        // start containing struct
        assert_success!(o_prot.write_struct_begin(&TStructIdentifier::new("foo")));

        // containing struct
        // since the delta is > 0 and < 15 it gets a delta write
        assert_success!(o_prot.write_field_begin(&TFieldIdentifier::new("foo", TType::I64, 1)));
        assert_success!(o_prot.write_field_end());

        // containing struct
        // since this delta > 0 and < 15 it gets a delta write
        assert_success!(o_prot.write_field_begin(&TFieldIdentifier::new("foo", TType::I32, 9)));
        assert_success!(o_prot.write_field_end());

        // start contained struct
        assert_success!(o_prot.write_struct_begin(&TStructIdentifier::new("foo")));

        // contained struct
        // since the delta is > 0 and < 15 it gets a delta write
        assert_success!(o_prot.write_field_begin(&TFieldIdentifier::new("foo", TType::I08, 7)));
        assert_success!(o_prot.write_field_end());

        // contained struct
        // since this delta > 15 it gets a full write
        assert_success!(o_prot.write_field_begin(&TFieldIdentifier::new("foo", TType::Double, 24)));
        assert_success!(o_prot.write_field_end());

        // end contained struct
        assert_success!(o_prot.write_field_stop());
        assert_success!(o_prot.write_struct_end());

        // end containing struct
        assert_success!(o_prot.write_field_stop());
        assert_success!(o_prot.write_struct_end());

        #[rustfmt::skip]
        let expected: [u8; 7] = [
            0x16, /* field delta (1) | field type */
            0x85, /* field delta (8) | field type */
            0x73, /* field delta (7) | field type */
            0x07, /* field type */
            0x30, /* zig-zag varint field id */
            0x00, /* field stop - contained */
            0x00 /* field stop - containing */,
        ];

        assert_eq_written_bytes!(o_prot, expected);
    }

    #[allow(clippy::cognitive_complexity)]
    #[test]
    fn must_round_trip_nested_structs_0() {
        // last field of the containing struct is a delta
        // first field of the the contained struct is a delta

        let (mut i_prot, mut o_prot) = test_objects();

        // start containing struct
        assert_success!(o_prot.write_struct_begin(&TStructIdentifier::new("foo")));

        // containing struct
        // since the delta is > 0 and < 15 it gets a delta write
        let field_ident_1 = TFieldIdentifier::new("foo", TType::I64, 1);
        assert_success!(o_prot.write_field_begin(&field_ident_1));
        assert_success!(o_prot.write_field_end());

        // containing struct
        // since this delta > 0 and < 15 it gets a delta write
        let field_ident_2 = TFieldIdentifier::new("foo", TType::I32, 9);
        assert_success!(o_prot.write_field_begin(&field_ident_2));
        assert_success!(o_prot.write_field_end());

        // start contained struct
        assert_success!(o_prot.write_struct_begin(&TStructIdentifier::new("foo")));

        // contained struct
        // since the delta is > 0 and < 15 it gets a delta write
        let field_ident_3 = TFieldIdentifier::new("foo", TType::I08, 7);
        assert_success!(o_prot.write_field_begin(&field_ident_3));
        assert_success!(o_prot.write_field_end());

        // contained struct
        // since this delta > 15 it gets a full write
        let field_ident_4 = TFieldIdentifier::new("foo", TType::Double, 24);
        assert_success!(o_prot.write_field_begin(&field_ident_4));
        assert_success!(o_prot.write_field_end());

        // end contained struct
        assert_success!(o_prot.write_field_stop());
        assert_success!(o_prot.write_struct_end());

        // end containing struct
        assert_success!(o_prot.write_field_stop());
        assert_success!(o_prot.write_struct_end());

        copy_write_buffer_to_read_buffer!(o_prot);

        // read containing struct back
        assert_success!(i_prot.read_struct_begin());

        let read_ident_1 = assert_success!(i_prot.read_field_begin());
        assert_eq!(
            read_ident_1,
            TFieldIdentifier {
                name: None,
                ..field_ident_1
            }
        );
        assert_success!(i_prot.read_field_end());

        let read_ident_2 = assert_success!(i_prot.read_field_begin());
        assert_eq!(
            read_ident_2,
            TFieldIdentifier {
                name: None,
                ..field_ident_2
            }
        );
        assert_success!(i_prot.read_field_end());

        // read contained struct back
        assert_success!(i_prot.read_struct_begin());

        let read_ident_3 = assert_success!(i_prot.read_field_begin());
        assert_eq!(
            read_ident_3,
            TFieldIdentifier {
                name: None,
                ..field_ident_3
            }
        );
        assert_success!(i_prot.read_field_end());

        let read_ident_4 = assert_success!(i_prot.read_field_begin());
        assert_eq!(
            read_ident_4,
            TFieldIdentifier {
                name: None,
                ..field_ident_4
            }
        );
        assert_success!(i_prot.read_field_end());

        // end contained struct
        let read_ident_6 = assert_success!(i_prot.read_field_begin());
        assert_eq!(
            read_ident_6,
            TFieldIdentifier {
                name: None,
                field_type: TType::Stop,
                id: None,
            }
        );
        assert_success!(i_prot.read_struct_end());

        // end containing struct
        let read_ident_7 = assert_success!(i_prot.read_field_begin());
        assert_eq!(
            read_ident_7,
            TFieldIdentifier {
                name: None,
                field_type: TType::Stop,
                id: None,
            }
        );
        assert_success!(i_prot.read_struct_end());
    }

    #[test]
    fn must_write_nested_structs_1() {
        // last field of the containing struct is a delta
        // first field of the the contained struct is a full write

        let (_, mut o_prot) = test_objects();

        // start containing struct
        assert_success!(o_prot.write_struct_begin(&TStructIdentifier::new("foo")));

        // containing struct
        // since the delta is > 0 and < 15 it gets a delta write
        assert_success!(o_prot.write_field_begin(&TFieldIdentifier::new("foo", TType::I64, 1)));
        assert_success!(o_prot.write_field_end());

        // containing struct
        // since this delta > 0 and < 15 it gets a delta write
        assert_success!(o_prot.write_field_begin(&TFieldIdentifier::new("foo", TType::I32, 9)));
        assert_success!(o_prot.write_field_end());

        // start contained struct
        assert_success!(o_prot.write_struct_begin(&TStructIdentifier::new("foo")));

        // contained struct
        // since this delta > 15 it gets a full write
        assert_success!(o_prot.write_field_begin(&TFieldIdentifier::new("foo", TType::Double, 24)));
        assert_success!(o_prot.write_field_end());

        // contained struct
        // since the delta is > 0 and < 15 it gets a delta write
        assert_success!(o_prot.write_field_begin(&TFieldIdentifier::new("foo", TType::I08, 27)));
        assert_success!(o_prot.write_field_end());

        // end contained struct
        assert_success!(o_prot.write_field_stop());
        assert_success!(o_prot.write_struct_end());

        // end containing struct
        assert_success!(o_prot.write_field_stop());
        assert_success!(o_prot.write_struct_end());

        #[rustfmt::skip]
        let expected: [u8; 7] = [
            0x16, /* field delta (1) | field type */
            0x85, /* field delta (8) | field type */
            0x07, /* field type */
            0x30, /* zig-zag varint field id */
            0x33, /* field delta (3) | field type */
            0x00, /* field stop - contained */
            0x00 /* field stop - containing */,
        ];

        assert_eq_written_bytes!(o_prot, expected);
    }

    #[allow(clippy::cognitive_complexity)]
    #[test]
    fn must_round_trip_nested_structs_1() {
        // last field of the containing struct is a delta
        // first field of the the contained struct is a full write

        let (mut i_prot, mut o_prot) = test_objects();

        // start containing struct
        assert_success!(o_prot.write_struct_begin(&TStructIdentifier::new("foo")));

        // containing struct
        // since the delta is > 0 and < 15 it gets a delta write
        let field_ident_1 = TFieldIdentifier::new("foo", TType::I64, 1);
        assert_success!(o_prot.write_field_begin(&field_ident_1));
        assert_success!(o_prot.write_field_end());

        // containing struct
        // since this delta > 0 and < 15 it gets a delta write
        let field_ident_2 = TFieldIdentifier::new("foo", TType::I32, 9);
        assert_success!(o_prot.write_field_begin(&field_ident_2));
        assert_success!(o_prot.write_field_end());

        // start contained struct
        assert_success!(o_prot.write_struct_begin(&TStructIdentifier::new("foo")));

        // contained struct
        // since this delta > 15 it gets a full write
        let field_ident_3 = TFieldIdentifier::new("foo", TType::Double, 24);
        assert_success!(o_prot.write_field_begin(&field_ident_3));
        assert_success!(o_prot.write_field_end());

        // contained struct
        // since the delta is > 0 and < 15 it gets a delta write
        let field_ident_4 = TFieldIdentifier::new("foo", TType::I08, 27);
        assert_success!(o_prot.write_field_begin(&field_ident_4));
        assert_success!(o_prot.write_field_end());

        // end contained struct
        assert_success!(o_prot.write_field_stop());
        assert_success!(o_prot.write_struct_end());

        // end containing struct
        assert_success!(o_prot.write_field_stop());
        assert_success!(o_prot.write_struct_end());

        copy_write_buffer_to_read_buffer!(o_prot);

        // read containing struct back
        assert_success!(i_prot.read_struct_begin());

        let read_ident_1 = assert_success!(i_prot.read_field_begin());
        assert_eq!(
            read_ident_1,
            TFieldIdentifier {
                name: None,
                ..field_ident_1
            }
        );
        assert_success!(i_prot.read_field_end());

        let read_ident_2 = assert_success!(i_prot.read_field_begin());
        assert_eq!(
            read_ident_2,
            TFieldIdentifier {
                name: None,
                ..field_ident_2
            }
        );
        assert_success!(i_prot.read_field_end());

        // read contained struct back
        assert_success!(i_prot.read_struct_begin());

        let read_ident_3 = assert_success!(i_prot.read_field_begin());
        assert_eq!(
            read_ident_3,
            TFieldIdentifier {
                name: None,
                ..field_ident_3
            }
        );
        assert_success!(i_prot.read_field_end());

        let read_ident_4 = assert_success!(i_prot.read_field_begin());
        assert_eq!(
            read_ident_4,
            TFieldIdentifier {
                name: None,
                ..field_ident_4
            }
        );
        assert_success!(i_prot.read_field_end());

        // end contained struct
        let read_ident_6 = assert_success!(i_prot.read_field_begin());
        assert_eq!(
            read_ident_6,
            TFieldIdentifier {
                name: None,
                field_type: TType::Stop,
                id: None,
            }
        );
        assert_success!(i_prot.read_struct_end());

        // end containing struct
        let read_ident_7 = assert_success!(i_prot.read_field_begin());
        assert_eq!(
            read_ident_7,
            TFieldIdentifier {
                name: None,
                field_type: TType::Stop,
                id: None,
            }
        );
        assert_success!(i_prot.read_struct_end());
    }

    #[test]
    fn must_write_nested_structs_2() {
        // last field of the containing struct is a full write
        // first field of the the contained struct is a delta write

        let (_, mut o_prot) = test_objects();

        // start containing struct
        assert_success!(o_prot.write_struct_begin(&TStructIdentifier::new("foo")));

        // containing struct
        // since the delta is > 0 and < 15 it gets a delta write
        assert_success!(o_prot.write_field_begin(&TFieldIdentifier::new("foo", TType::I64, 1)));
        assert_success!(o_prot.write_field_end());

        // containing struct
        // since this delta > 15 it gets a full write
        assert_success!(o_prot.write_field_begin(&TFieldIdentifier::new("foo", TType::String, 21)));
        assert_success!(o_prot.write_field_end());

        // start contained struct
        assert_success!(o_prot.write_struct_begin(&TStructIdentifier::new("foo")));

        // contained struct
        // since this delta > 0 and < 15 it gets a delta write
        assert_success!(o_prot.write_field_begin(&TFieldIdentifier::new("foo", TType::Double, 7)));
        assert_success!(o_prot.write_field_end());

        // contained struct
        // since the delta is > 0 and < 15 it gets a delta write
        assert_success!(o_prot.write_field_begin(&TFieldIdentifier::new("foo", TType::I08, 10)));
        assert_success!(o_prot.write_field_end());

        // end contained struct
        assert_success!(o_prot.write_field_stop());
        assert_success!(o_prot.write_struct_end());

        // end containing struct
        assert_success!(o_prot.write_field_stop());
        assert_success!(o_prot.write_struct_end());

        #[rustfmt::skip]
        let expected: [u8; 7] = [
            0x16, /* field delta (1) | field type */
            0x08, /* field type */
            0x2A, /* zig-zag varint field id */
            0x77, /* field delta(7) | field type */
            0x33, /* field delta (3) | field type */
            0x00, /* field stop - contained */
            0x00 /* field stop - containing */,
        ];

        assert_eq_written_bytes!(o_prot, expected);
    }

    #[allow(clippy::cognitive_complexity)]
    #[test]
    fn must_round_trip_nested_structs_2() {
        let (mut i_prot, mut o_prot) = test_objects();

        // start containing struct
        assert_success!(o_prot.write_struct_begin(&TStructIdentifier::new("foo")));

        // containing struct
        // since the delta is > 0 and < 15 it gets a delta write
        let field_ident_1 = TFieldIdentifier::new("foo", TType::I64, 1);
        assert_success!(o_prot.write_field_begin(&field_ident_1));
        assert_success!(o_prot.write_field_end());

        // containing struct
        // since this delta > 15 it gets a full write
        let field_ident_2 = TFieldIdentifier::new("foo", TType::String, 21);
        assert_success!(o_prot.write_field_begin(&field_ident_2));
        assert_success!(o_prot.write_field_end());

        // start contained struct
        assert_success!(o_prot.write_struct_begin(&TStructIdentifier::new("foo")));

        // contained struct
        // since this delta > 0 and < 15 it gets a delta write
        let field_ident_3 = TFieldIdentifier::new("foo", TType::Double, 7);
        assert_success!(o_prot.write_field_begin(&field_ident_3));
        assert_success!(o_prot.write_field_end());

        // contained struct
        // since the delta is > 0 and < 15 it gets a delta write
        let field_ident_4 = TFieldIdentifier::new("foo", TType::I08, 10);
        assert_success!(o_prot.write_field_begin(&field_ident_4));
        assert_success!(o_prot.write_field_end());

        // end contained struct
        assert_success!(o_prot.write_field_stop());
        assert_success!(o_prot.write_struct_end());

        // end containing struct
        assert_success!(o_prot.write_field_stop());
        assert_success!(o_prot.write_struct_end());

        copy_write_buffer_to_read_buffer!(o_prot);

        // read containing struct back
        assert_success!(i_prot.read_struct_begin());

        let read_ident_1 = assert_success!(i_prot.read_field_begin());
        assert_eq!(
            read_ident_1,
            TFieldIdentifier {
                name: None,
                ..field_ident_1
            }
        );
        assert_success!(i_prot.read_field_end());

        let read_ident_2 = assert_success!(i_prot.read_field_begin());
        assert_eq!(
            read_ident_2,
            TFieldIdentifier {
                name: None,
                ..field_ident_2
            }
        );
        assert_success!(i_prot.read_field_end());

        // read contained struct back
        assert_success!(i_prot.read_struct_begin());

        let read_ident_3 = assert_success!(i_prot.read_field_begin());
        assert_eq!(
            read_ident_3,
            TFieldIdentifier {
                name: None,
                ..field_ident_3
            }
        );
        assert_success!(i_prot.read_field_end());

        let read_ident_4 = assert_success!(i_prot.read_field_begin());
        assert_eq!(
            read_ident_4,
            TFieldIdentifier {
                name: None,
                ..field_ident_4
            }
        );
        assert_success!(i_prot.read_field_end());

        // end contained struct
        let read_ident_6 = assert_success!(i_prot.read_field_begin());
        assert_eq!(
            read_ident_6,
            TFieldIdentifier {
                name: None,
                field_type: TType::Stop,
                id: None,
            }
        );
        assert_success!(i_prot.read_struct_end());

        // end containing struct
        let read_ident_7 = assert_success!(i_prot.read_field_begin());
        assert_eq!(
            read_ident_7,
            TFieldIdentifier {
                name: None,
                field_type: TType::Stop,
                id: None,
            }
        );
        assert_success!(i_prot.read_struct_end());
    }

    #[test]
    fn must_write_nested_structs_3() {
        // last field of the containing struct is a full write
        // first field of the the contained struct is a full write

        let (_, mut o_prot) = test_objects();

        // start containing struct
        assert_success!(o_prot.write_struct_begin(&TStructIdentifier::new("foo")));

        // containing struct
        // since the delta is > 0 and < 15 it gets a delta write
        assert_success!(o_prot.write_field_begin(&TFieldIdentifier::new("foo", TType::I64, 1)));
        assert_success!(o_prot.write_field_end());

        // containing struct
        // since this delta > 15 it gets a full write
        assert_success!(o_prot.write_field_begin(&TFieldIdentifier::new("foo", TType::String, 21)));
        assert_success!(o_prot.write_field_end());

        // start contained struct
        assert_success!(o_prot.write_struct_begin(&TStructIdentifier::new("foo")));

        // contained struct
        // since this delta > 15 it gets a full write
        assert_success!(o_prot.write_field_begin(&TFieldIdentifier::new("foo", TType::Double, 21)));
        assert_success!(o_prot.write_field_end());

        // contained struct
        // since the delta is > 0 and < 15 it gets a delta write
        assert_success!(o_prot.write_field_begin(&TFieldIdentifier::new("foo", TType::I08, 27)));
        assert_success!(o_prot.write_field_end());

        // end contained struct
        assert_success!(o_prot.write_field_stop());
        assert_success!(o_prot.write_struct_end());

        // end containing struct
        assert_success!(o_prot.write_field_stop());
        assert_success!(o_prot.write_struct_end());

        #[rustfmt::skip]
        let expected: [u8; 8] = [
            0x16, /* field delta (1) | field type */
            0x08, /* field type */
            0x2A, /* zig-zag varint field id */
            0x07, /* field type */
            0x2A, /* zig-zag varint field id */
            0x63, /* field delta (6) | field type */
            0x00, /* field stop - contained */
            0x00 /* field stop - containing */,
        ];

        assert_eq_written_bytes!(o_prot, expected);
    }

    #[allow(clippy::cognitive_complexity)]
    #[test]
    fn must_round_trip_nested_structs_3() {
        // last field of the containing struct is a full write
        // first field of the the contained struct is a full write

        let (mut i_prot, mut o_prot) = test_objects();

        // start containing struct
        assert_success!(o_prot.write_struct_begin(&TStructIdentifier::new("foo")));

        // containing struct
        // since the delta is > 0 and < 15 it gets a delta write
        let field_ident_1 = TFieldIdentifier::new("foo", TType::I64, 1);
        assert_success!(o_prot.write_field_begin(&field_ident_1));
        assert_success!(o_prot.write_field_end());

        // containing struct
        // since this delta > 15 it gets a full write
        let field_ident_2 = TFieldIdentifier::new("foo", TType::String, 21);
        assert_success!(o_prot.write_field_begin(&field_ident_2));
        assert_success!(o_prot.write_field_end());

        // start contained struct
        assert_success!(o_prot.write_struct_begin(&TStructIdentifier::new("foo")));

        // contained struct
        // since this delta > 15 it gets a full write
        let field_ident_3 = TFieldIdentifier::new("foo", TType::Double, 21);
        assert_success!(o_prot.write_field_begin(&field_ident_3));
        assert_success!(o_prot.write_field_end());

        // contained struct
        // since the delta is > 0 and < 15 it gets a delta write
        let field_ident_4 = TFieldIdentifier::new("foo", TType::I08, 27);
        assert_success!(o_prot.write_field_begin(&field_ident_4));
        assert_success!(o_prot.write_field_end());

        // end contained struct
        assert_success!(o_prot.write_field_stop());
        assert_success!(o_prot.write_struct_end());

        // end containing struct
        assert_success!(o_prot.write_field_stop());
        assert_success!(o_prot.write_struct_end());

        copy_write_buffer_to_read_buffer!(o_prot);

        // read containing struct back
        assert_success!(i_prot.read_struct_begin());

        let read_ident_1 = assert_success!(i_prot.read_field_begin());
        assert_eq!(
            read_ident_1,
            TFieldIdentifier {
                name: None,
                ..field_ident_1
            }
        );
        assert_success!(i_prot.read_field_end());

        let read_ident_2 = assert_success!(i_prot.read_field_begin());
        assert_eq!(
            read_ident_2,
            TFieldIdentifier {
                name: None,
                ..field_ident_2
            }
        );
        assert_success!(i_prot.read_field_end());

        // read contained struct back
        assert_success!(i_prot.read_struct_begin());

        let read_ident_3 = assert_success!(i_prot.read_field_begin());
        assert_eq!(
            read_ident_3,
            TFieldIdentifier {
                name: None,
                ..field_ident_3
            }
        );
        assert_success!(i_prot.read_field_end());

        let read_ident_4 = assert_success!(i_prot.read_field_begin());
        assert_eq!(
            read_ident_4,
            TFieldIdentifier {
                name: None,
                ..field_ident_4
            }
        );
        assert_success!(i_prot.read_field_end());

        // end contained struct
        let read_ident_6 = assert_success!(i_prot.read_field_begin());
        assert_eq!(
            read_ident_6,
            TFieldIdentifier {
                name: None,
                field_type: TType::Stop,
                id: None,
            }
        );
        assert_success!(i_prot.read_struct_end());

        // end containing struct
        let read_ident_7 = assert_success!(i_prot.read_field_begin());
        assert_eq!(
            read_ident_7,
            TFieldIdentifier {
                name: None,
                field_type: TType::Stop,
                id: None,
            }
        );
        assert_success!(i_prot.read_struct_end());
    }

    #[test]
    fn must_write_bool_field() {
        let (_, mut o_prot) = test_objects();

        // no bytes should be written however
        assert_success!(o_prot.write_struct_begin(&TStructIdentifier::new("foo")));

        // write three fields with field ids that cannot be encoded as deltas

        // since the delta is > 0 and < 16 it gets a delta write
        assert_success!(o_prot.write_field_begin(&TFieldIdentifier::new("foo", TType::Bool, 1)));
        assert_success!(o_prot.write_bool(true));
        assert_success!(o_prot.write_field_end());

        // since this delta > 0 and < 15 it gets a delta write
        assert_success!(o_prot.write_field_begin(&TFieldIdentifier::new("foo", TType::Bool, 9)));
        assert_success!(o_prot.write_bool(false));
        assert_success!(o_prot.write_field_end());

        // since this delta > 15 it gets a full write
        assert_success!(o_prot.write_field_begin(&TFieldIdentifier::new("foo", TType::Bool, 26)));
        assert_success!(o_prot.write_bool(true));
        assert_success!(o_prot.write_field_end());

        // since this delta > 15 it gets a full write
        assert_success!(o_prot.write_field_begin(&TFieldIdentifier::new("foo", TType::Bool, 45)));
        assert_success!(o_prot.write_bool(false));
        assert_success!(o_prot.write_field_end());

        // now, finish the struct off
        assert_success!(o_prot.write_field_stop());
        assert_success!(o_prot.write_struct_end());

        #[rustfmt::skip]
        let expected: [u8; 7] = [
            0x11, /* field delta (1) | true */
            0x82, /* field delta (8) | false */
            0x01, /* true */
            0x34, /* field id */
            0x02, /* false */
            0x5A, /* field id */
            0x00 /* stop field */,
        ];

        assert_eq_written_bytes!(o_prot, expected);
    }

    #[allow(clippy::cognitive_complexity)]
    #[test]
    fn must_round_trip_bool_field() {
        let (mut i_prot, mut o_prot) = test_objects();

        // no bytes should be written however
        let struct_ident = TStructIdentifier::new("foo");
        assert_success!(o_prot.write_struct_begin(&struct_ident));

        // write two fields

        // since the delta is > 0 and < 16 it gets a delta write
        let field_ident_1 = TFieldIdentifier::new("foo", TType::Bool, 1);
        assert_success!(o_prot.write_field_begin(&field_ident_1));
        assert_success!(o_prot.write_bool(true));
        assert_success!(o_prot.write_field_end());

        // since this delta > 0 and < 15 it gets a delta write
        let field_ident_2 = TFieldIdentifier::new("foo", TType::Bool, 9);
        assert_success!(o_prot.write_field_begin(&field_ident_2));
        assert_success!(o_prot.write_bool(false));
        assert_success!(o_prot.write_field_end());

        // since this delta > 15 it gets a full write
        let field_ident_3 = TFieldIdentifier::new("foo", TType::Bool, 26);
        assert_success!(o_prot.write_field_begin(&field_ident_3));
        assert_success!(o_prot.write_bool(true));
        assert_success!(o_prot.write_field_end());

        // since this delta > 15 it gets a full write
        let field_ident_4 = TFieldIdentifier::new("foo", TType::Bool, 45);
        assert_success!(o_prot.write_field_begin(&field_ident_4));
        assert_success!(o_prot.write_bool(false));
        assert_success!(o_prot.write_field_end());

        // now, finish the struct off
        assert_success!(o_prot.write_field_stop());
        assert_success!(o_prot.write_struct_end());

        copy_write_buffer_to_read_buffer!(o_prot);

        // read the struct back
        assert_success!(i_prot.read_struct_begin());

        let read_ident_1 = assert_success!(i_prot.read_field_begin());
        assert_eq!(
            read_ident_1,
            TFieldIdentifier {
                name: None,
                ..field_ident_1
            }
        );
        let read_value_1 = assert_success!(i_prot.read_bool());
        assert!(read_value_1);
        assert_success!(i_prot.read_field_end());

        let read_ident_2 = assert_success!(i_prot.read_field_begin());
        assert_eq!(
            read_ident_2,
            TFieldIdentifier {
                name: None,
                ..field_ident_2
            }
        );
        let read_value_2 = assert_success!(i_prot.read_bool());
        assert!(!read_value_2);
        assert_success!(i_prot.read_field_end());

        let read_ident_3 = assert_success!(i_prot.read_field_begin());
        assert_eq!(
            read_ident_3,
            TFieldIdentifier {
                name: None,
                ..field_ident_3
            }
        );
        let read_value_3 = assert_success!(i_prot.read_bool());
        assert!(read_value_3);
        assert_success!(i_prot.read_field_end());

        let read_ident_4 = assert_success!(i_prot.read_field_begin());
        assert_eq!(
            read_ident_4,
            TFieldIdentifier {
                name: None,
                ..field_ident_4
            }
        );
        let read_value_4 = assert_success!(i_prot.read_bool());
        assert!(!read_value_4);
        assert_success!(i_prot.read_field_end());

        let read_ident_5 = assert_success!(i_prot.read_field_begin());
        assert_eq!(
            read_ident_5,
            TFieldIdentifier {
                name: None,
                field_type: TType::Stop,
                id: None,
            }
        );

        assert_success!(i_prot.read_struct_end());
    }

    #[test]
    #[should_panic]
    fn must_fail_if_write_field_end_without_writing_bool_value() {
        let (_, mut o_prot) = test_objects();
        assert_success!(o_prot.write_struct_begin(&TStructIdentifier::new("foo")));
        assert_success!(o_prot.write_field_begin(&TFieldIdentifier::new("foo", TType::Bool, 1)));
        o_prot.write_field_end().unwrap();
    }

    #[test]
    #[should_panic]
    fn must_fail_if_write_stop_field_without_writing_bool_value() {
        let (_, mut o_prot) = test_objects();
        assert_success!(o_prot.write_struct_begin(&TStructIdentifier::new("foo")));
        assert_success!(o_prot.write_field_begin(&TFieldIdentifier::new("foo", TType::Bool, 1)));
        o_prot.write_field_stop().unwrap();
    }

    #[test]
    #[should_panic]
    fn must_fail_if_write_struct_end_without_writing_bool_value() {
        let (_, mut o_prot) = test_objects();
        assert_success!(o_prot.write_struct_begin(&TStructIdentifier::new("foo")));
        assert_success!(o_prot.write_field_begin(&TFieldIdentifier::new("foo", TType::Bool, 1)));
        o_prot.write_struct_end().unwrap();
    }

    #[test]
    #[should_panic]
    fn must_fail_if_write_struct_end_without_any_fields() {
        let (_, mut o_prot) = test_objects();
        o_prot.write_struct_end().unwrap();
    }

    #[test]
    fn must_write_field_end() {
        assert_no_write(|o| o.write_field_end());
    }

    #[test]
    fn must_write_small_sized_list_begin() {
        let (_, mut o_prot) = test_objects();

        assert_success!(o_prot.write_list_begin(&TListIdentifier::new(TType::I64, 4)));

        let expected: [u8; 1] = [0x46 /* size | elem_type */];

        assert_eq_written_bytes!(o_prot, expected);
    }

    #[test]
    fn must_round_trip_small_sized_list_begin() {
        let (mut i_prot, mut o_prot) = test_objects();

        let ident = TListIdentifier::new(TType::I32, 3);
        assert_success!(o_prot.write_list_begin(&ident));

        assert_success!(o_prot.write_i32(100));
        assert_success!(o_prot.write_i32(200));
        assert_success!(o_prot.write_i32(300));

        assert_success!(o_prot.write_list_end());

        copy_write_buffer_to_read_buffer!(o_prot);

        let res = assert_success!(i_prot.read_list_begin());
        assert_eq!(&res, &ident);

        assert_eq!(i_prot.read_i32().unwrap(), 100);
        assert_eq!(i_prot.read_i32().unwrap(), 200);
        assert_eq!(i_prot.read_i32().unwrap(), 300);

        assert_success!(i_prot.read_list_end());
    }

    #[test]
    fn must_write_large_sized_list_begin() {
        let (_, mut o_prot) = test_objects();

        let res = o_prot.write_list_begin(&TListIdentifier::new(TType::List, 9999));
        assert!(res.is_ok());

        let expected: [u8; 3] = [
            0xF9, /* 0xF0 | elem_type */
            0x8F, 0x4E, /* size as varint */
        ];

        assert_eq_written_bytes!(o_prot, expected);
    }

    #[test]
    fn must_round_trip_large_sized_list_begin() {
        let (mut i_prot, mut o_prot) = test_objects_no_limits();

        let ident = TListIdentifier::new(TType::Set, 47381);
        assert_success!(o_prot.write_list_begin(&ident));

        copy_write_buffer_to_read_buffer!(o_prot);

        let res = assert_success!(i_prot.read_list_begin());
        assert_eq!(&res, &ident);
    }

    #[test]
    fn must_write_list_end() {
        assert_no_write(|o| o.write_list_end());
    }

    #[test]
    fn must_write_small_sized_set_begin() {
        let (_, mut o_prot) = test_objects();

        assert_success!(o_prot.write_set_begin(&TSetIdentifier::new(TType::Struct, 2)));

        let expected: [u8; 1] = [0x2C /* size | elem_type */];

        assert_eq_written_bytes!(o_prot, expected);
    }

    #[test]
    fn must_round_trip_small_sized_set_begin() {
        let (mut i_prot, mut o_prot) = test_objects();

        let ident = TSetIdentifier::new(TType::I16, 3);
        assert_success!(o_prot.write_set_begin(&ident));

        assert_success!(o_prot.write_i16(111));
        assert_success!(o_prot.write_i16(222));
        assert_success!(o_prot.write_i16(333));

        assert_success!(o_prot.write_set_end());

        copy_write_buffer_to_read_buffer!(o_prot);

        let res = assert_success!(i_prot.read_set_begin());
        assert_eq!(&res, &ident);

        assert_eq!(i_prot.read_i16().unwrap(), 111);
        assert_eq!(i_prot.read_i16().unwrap(), 222);
        assert_eq!(i_prot.read_i16().unwrap(), 333);

        assert_success!(i_prot.read_set_end());
    }

    #[test]
    fn must_write_large_sized_set_begin() {
        let (_, mut o_prot) = test_objects();

        assert_success!(o_prot.write_set_begin(&TSetIdentifier::new(TType::Double, 23891)));

        let expected: [u8; 4] = [
            0xF7, /* 0xF0 | elem_type */
            0xD3, 0xBA, 0x01, /* size as varint */
        ];

        assert_eq_written_bytes!(o_prot, expected);
    }

    #[test]
    fn must_round_trip_large_sized_set_begin() {
        let (mut i_prot, mut o_prot) = test_objects_no_limits();

        let ident = TSetIdentifier::new(TType::Map, 3_928_429);
        assert_success!(o_prot.write_set_begin(&ident));

        copy_write_buffer_to_read_buffer!(o_prot);

        let res = assert_success!(i_prot.read_set_begin());
        assert_eq!(&res, &ident);
    }

    #[test]
    fn must_write_set_end() {
        assert_no_write(|o| o.write_set_end());
    }

    #[test]
    fn must_write_zero_sized_map_begin() {
        let (_, mut o_prot) = test_objects();

        assert_success!(o_prot.write_map_begin(&TMapIdentifier::new(TType::String, TType::I32, 0)));

        let expected: [u8; 1] = [0x00]; // since size is zero we don't write anything

        assert_eq_written_bytes!(o_prot, expected);
    }

    #[test]
    fn must_read_zero_sized_map_begin() {
        let (mut i_prot, mut o_prot) = test_objects();

        assert_success!(o_prot.write_map_begin(&TMapIdentifier::new(TType::Double, TType::I32, 0)));

        copy_write_buffer_to_read_buffer!(o_prot);

        let res = assert_success!(i_prot.read_map_begin());
        assert_eq!(
            &res,
            &TMapIdentifier {
                key_type: None,
                value_type: None,
                size: 0,
            }
        );
    }

    #[test]
    fn must_write_map_begin() {
        let (_, mut o_prot) = test_objects();

        assert_success!(o_prot.write_map_begin(&TMapIdentifier::new(
            TType::Double,
            TType::String,
            238
        )));

        let expected: [u8; 3] = [
            0xEE, 0x01, /* size as varint */
            0x78, /* key type | val type */
        ];

        assert_eq_written_bytes!(o_prot, expected);
    }

    #[test]
    fn must_round_trip_map_begin() {
        let (mut i_prot, mut o_prot) = test_objects_no_limits();

        let ident = TMapIdentifier::new(TType::Map, TType::List, 1_928_349);
        assert_success!(o_prot.write_map_begin(&ident));

        copy_write_buffer_to_read_buffer!(o_prot);

        let res = assert_success!(i_prot.read_map_begin());
        assert_eq!(&res, &ident);
    }

    #[test]
    fn must_write_map_end() {
        assert_no_write(|o| o.write_map_end());
    }

    #[test]
    fn must_write_map_with_bool_key_and_value() {
        let (_, mut o_prot) = test_objects();

        assert_success!(o_prot.write_map_begin(&TMapIdentifier::new(TType::Bool, TType::Bool, 1)));
        assert_success!(o_prot.write_bool(true));
        assert_success!(o_prot.write_bool(false));
        assert_success!(o_prot.write_map_end());

        let expected: [u8; 4] = [
            0x01, /* size as varint */
            0x11, /* key type | val type */
            0x01, /* key: true */
            0x02, /* val: false */
        ];

        assert_eq_written_bytes!(o_prot, expected);
    }

    #[test]
    fn must_round_trip_map_with_bool_value() {
        let (mut i_prot, mut o_prot) = test_objects();

        let map_ident = TMapIdentifier::new(TType::Bool, TType::Bool, 2);
        assert_success!(o_prot.write_map_begin(&map_ident));
        assert_success!(o_prot.write_bool(true));
        assert_success!(o_prot.write_bool(false));
        assert_success!(o_prot.write_bool(false));
        assert_success!(o_prot.write_bool(true));
        assert_success!(o_prot.write_map_end());

        copy_write_buffer_to_read_buffer!(o_prot);

        // map header
        let rcvd_ident = assert_success!(i_prot.read_map_begin());
        assert_eq!(&rcvd_ident, &map_ident);
        // key 1
        let b = assert_success!(i_prot.read_bool());
        assert!(b);
        // val 1
        let b = assert_success!(i_prot.read_bool());
        assert!(!b);
        // key 2
        let b = assert_success!(i_prot.read_bool());
        assert!(!b);
        // val 2
        let b = assert_success!(i_prot.read_bool());
        assert!(b);
        // map end
        assert_success!(i_prot.read_map_end());
    }

    #[test]
    fn must_read_map_end() {
        let (mut i_prot, _) = test_objects();
        assert!(i_prot.read_map_end().is_ok()); // will blow up if we try to read from empty buffer
    }

    fn test_objects() -> (
        TCompactInputProtocol<ReadHalf<TBufferChannel>>,
        TCompactOutputProtocol<WriteHalf<TBufferChannel>>,
    ) {
        let mem = TBufferChannel::with_capacity(200, 200);

        let (r_mem, w_mem) = mem.split().unwrap();

        let i_prot = TCompactInputProtocol::new(r_mem);
        let o_prot = TCompactOutputProtocol::new(w_mem);

        (i_prot, o_prot)
    }

    fn test_objects_no_limits() -> (
        TCompactInputProtocol<ReadHalf<TBufferChannel>>,
        TCompactOutputProtocol<WriteHalf<TBufferChannel>>,
    ) {
        let mem = TBufferChannel::with_capacity(200, 200);

        let (r_mem, w_mem) = mem.split().unwrap();

        let i_prot = TCompactInputProtocol::with_config(r_mem, TConfiguration::no_limits());
        let o_prot = TCompactOutputProtocol::new(w_mem);

        (i_prot, o_prot)
    }

    #[test]
    fn must_read_write_double() {
        let (mut i_prot, mut o_prot) = test_objects();

        #[allow(clippy::approx_constant)]
        let double = 3.141_592_653_589_793;
        o_prot.write_double(double).unwrap();
        copy_write_buffer_to_read_buffer!(o_prot);

        let read_double = i_prot.read_double().unwrap();
        assert!((read_double - double).abs() < f64::EPSILON);
    }

    #[test]
    fn must_encode_double_as_other_langs() {
        let (_, mut o_prot) = test_objects();
        let expected = [24, 45, 68, 84, 251, 33, 9, 64];

        #[allow(clippy::approx_constant)]
        let double = 3.141_592_653_589_793;
        o_prot.write_double(double).unwrap();

        assert_eq_written_bytes!(o_prot, expected);
    }

    fn assert_no_write<F>(mut write_fn: F)
    where
        F: FnMut(&mut TCompactOutputProtocol<WriteHalf<TBufferChannel>>) -> crate::Result<()>,
    {
        let (_, mut o_prot) = test_objects();
        assert!(write_fn(&mut o_prot).is_ok());
        assert_eq!(o_prot.transport.write_bytes().len(), 0);
    }

    #[test]
    fn must_read_boolean_list() {
        let (mut i_prot, _) = test_objects();

        let source_bytes: [u8; 3] = [0x21, 2, 1];

        i_prot.transport.set_readable_bytes(&source_bytes);

        let (ttype, element_count) = assert_success!(i_prot.read_list_set_begin());

        assert_eq!(ttype, TType::Bool);
        assert_eq!(element_count, 2);
        assert_eq!(i_prot.read_bool().unwrap(), false);
        assert_eq!(i_prot.read_bool().unwrap(), true);

        assert_success!(i_prot.read_list_end());
    }

    #[test]
    fn must_read_boolean_list_alternative_encoding() {
        let (mut i_prot, _) = test_objects();

        let source_bytes: [u8; 3] = [0x22, 0, 1];

        i_prot.transport.set_readable_bytes(&source_bytes);

        let (ttype, element_count) = assert_success!(i_prot.read_list_set_begin());

        assert_eq!(ttype, TType::Bool);
        assert_eq!(element_count, 2);
        assert_eq!(i_prot.read_bool().unwrap(), false);
        assert_eq!(i_prot.read_bool().unwrap(), true);

        assert_success!(i_prot.read_list_end());
    }

    #[test]
    fn must_enforce_recursion_depth_limit() {
        let channel = TBufferChannel::with_capacity(100, 100);

        // Create a configuration with a small recursion limit
        let config = TConfiguration::builder()
            .max_recursion_depth(Some(2))
            .build()
            .unwrap();

        let mut protocol = TCompactInputProtocol::with_config(channel, config);

        // First struct - should succeed
        assert!(protocol.read_struct_begin().is_ok());

        // Second struct - should succeed (at limit)
        assert!(protocol.read_struct_begin().is_ok());

        // Third struct - should fail (exceeds limit)
        let result = protocol.read_struct_begin();
        assert!(result.is_err());
        match result {
            Err(crate::Error::Protocol(e)) => {
                assert_eq!(e.kind, ProtocolErrorKind::DepthLimit);
            }
            _ => panic!("Expected protocol error with DepthLimit"),
        }
    }

    #[test]
    fn must_check_container_size_overflow() {
        // Configure a small message size limit
        let config = TConfiguration::builder()
            .max_message_size(Some(1000))
            .max_frame_size(Some(1000))
            .build()
            .unwrap();
        let transport = TBufferChannel::with_capacity(100, 0);
        let mut i_prot = TCompactInputProtocol::with_config(transport, config);

        // Write a list header that would require more memory than message size limit
        // List of 100 UUIDs (16 bytes each) = 1600 bytes > 1000 limit
        i_prot.transport.set_readable_bytes(&[
            0xFD, // element type UUID (0x0D) | count in next bytes (0xF0)
            0x64, // varint 100
        ]);

        let result = i_prot.read_list_begin();
        assert!(result.is_err());
        match result {
            Err(crate::Error::Protocol(e)) => {
                assert_eq!(e.kind, ProtocolErrorKind::SizeLimit);
                assert!(e
                    .message
                    .contains("1600 bytes, exceeding message size limit of 1000"));
            }
            _ => panic!("Expected protocol error with SizeLimit"),
        }
    }

    #[test]
    fn must_reject_negative_container_sizes() {
        let mut channel = TBufferChannel::with_capacity(100, 100);

        let mut protocol = TCompactInputProtocol::new(channel.clone());

        // Write header with negative size when decoded
        // In compact protocol, lists/sets use a header byte followed by size
        // We'll use 0x0F for element type and then a varint-encoded negative number
        channel.set_readable_bytes(&[
            0xF0, // Header: 15 in upper nibble (triggers varint read), List type in lower
            0xFF, 0xFF, 0xFF, 0xFF, 0x0F, // Varint encoding of -1
        ]);

        let result = protocol.read_list_begin();
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
        let channel = TBufferChannel::with_capacity(100, 100);
        let (r_channel, mut w_channel) = channel.split().unwrap();

        // Create protocol with explicit container size limit
        let config = TConfiguration::builder()
            .max_container_size(Some(1000))
            .build()
            .unwrap();
        let mut protocol = TCompactInputProtocol::with_config(r_channel, config);

        // Write header with large size
        // Compact protocol: 0xF0 means size >= 15 is encoded as varint
        // Then we write a varint encoding 10000 (exceeds our limit of 1000)
        w_channel.set_readable_bytes(&[
            0xF0, // Header: 15 in upper nibble (triggers varint read), element type in lower
            0x90, 0x4E, // Varint encoding of 10000
        ]);

        let result = protocol.read_list_begin();
        assert!(result.is_err());
        match result {
            Err(crate::Error::Protocol(e)) => {
                assert_eq!(e.kind, ProtocolErrorKind::SizeLimit);
                assert!(e.message.contains("exceeds maximum allowed size"));
            }
            _ => panic!("Expected protocol error with SizeLimit"),
        }
    }

    #[test]
    fn must_handle_varint_size_overflow() {
        // Test that compact protocol properly handles varint-encoded sizes that would cause overflow
        let mut channel = TBufferChannel::with_capacity(100, 100);

        let mut protocol = TCompactInputProtocol::new(channel.clone());

        // Create input that encodes a very large size using varint encoding
        // 0xFA = list header with size >= 15 (so size follows as varint)
        // Then multiple 0xFF bytes which in varint encoding create a very large number
        channel.set_readable_bytes(&[
            0xFA, // List header: size >= 15, element type = 0x0A
            0xFF, 0xFF, 0xFF, 0xFF, 0x7F, // Varint encoding of a huge number
        ]);

        let result = protocol.read_list_begin();
        assert!(result.is_err());
        match result {
            Err(crate::Error::Protocol(e)) => {
                // The varint decoder might interpret this as negative, which is also fine
                assert!(
                    e.kind == ProtocolErrorKind::SizeLimit
                        || e.kind == ProtocolErrorKind::NegativeSize,
                    "Expected SizeLimit or NegativeSize but got {:?}",
                    e.kind
                );
            }
            _ => panic!("Expected protocol error"),
        }
    }

    #[test]
    fn must_enforce_string_size_limit() {
        let channel = TBufferChannel::with_capacity(100, 100);
        let (r_channel, mut w_channel) = channel.split().unwrap();

        // Create protocol with string limit of 100 bytes
        let config = TConfiguration::builder()
            .max_string_size(Some(100))
            .build()
            .unwrap();
        let mut protocol = TCompactInputProtocol::with_config(r_channel, config);

        // Write a varint-encoded string size that exceeds the limit
        w_channel.set_readable_bytes(&[
            0xC8, 0x01, // Varint encoding of 200
        ]);

        let result = protocol.read_string();
        assert!(result.is_err());
        match result {
            Err(crate::Error::Protocol(e)) => {
                assert_eq!(e.kind, ProtocolErrorKind::SizeLimit);
                assert!(e.message.contains("exceeds maximum allowed size"));
            }
            _ => panic!("Expected protocol error with SizeLimit"),
        }
    }

    #[test]
    fn must_allow_no_limit_configuration() {
        let channel = TBufferChannel::with_capacity(40, 40);

        let config = TConfiguration::no_limits();
        let mut protocol = TCompactInputProtocol::with_config(channel, config);

        // Should be able to nest structs deeply without limit
        for _ in 0..100 {
            assert!(protocol.read_struct_begin().is_ok());
        }

        for _ in 0..100 {
            assert!(protocol.read_struct_end().is_ok());
        }
    }

    #[test]
    fn must_allow_containers_within_limit() {
        let channel = TBufferChannel::with_capacity(200, 200);
        let (r_channel, mut w_channel) = channel.split().unwrap();

        // Create protocol with container limit of 100
        let config = TConfiguration::builder()
            .max_container_size(Some(100))
            .build()
            .unwrap();
        let mut protocol = TCompactInputProtocol::with_config(r_channel, config);

        // Write a list with 5 i32 elements (well within limit of 100)
        // Compact protocol: size < 15 is encoded in header
        w_channel.set_readable_bytes(&[
            0x55, // Header: size=5, element type=5 (i32)
            // 5 varint-encoded i32 values
            0x0A, // 10
            0x14, // 20
            0x1E, // 30
            0x28, // 40
            0x32, // 50
        ]);

        let result = protocol.read_list_begin();
        assert!(result.is_ok());
        let list_ident = result.unwrap();
        assert_eq!(list_ident.size, 5);
        assert_eq!(list_ident.element_type, TType::I32);
    }

    #[test]
    fn must_allow_strings_within_limit() {
        let channel = TBufferChannel::with_capacity(100, 100);
        let (r_channel, mut w_channel) = channel.split().unwrap();

        let config = TConfiguration::builder()
            .max_string_size(Some(1000))
            .build()
            .unwrap();
        let mut protocol = TCompactInputProtocol::with_config(r_channel, config);

        // Write a string "hello" (5 bytes, well within limit)
        w_channel.set_readable_bytes(&[
            0x05, // Varint-encoded length: 5
            b'h', b'e', b'l', b'l', b'o',
        ]);

        let result = protocol.read_string();
        assert!(result.is_ok());
        assert_eq!(result.unwrap(), "hello");
    }
}
