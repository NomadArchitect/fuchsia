// Copyright 2024 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use core::mem::MaybeUninit;

use munge::munge;

use crate::decoder::InternalHandleDecoder;
use crate::encoder::InternalHandleEncoder;
use crate::{
    Decode, DecodeError, Decoder, Encode, EncodeError, Encoder, Slot, WireEnvelope, WireU64,
    ZeroPadding,
};

/// A raw FIDL union
#[repr(C)]
pub struct RawWireUnion {
    ordinal: WireU64,
    envelope: WireEnvelope,
}

unsafe impl ZeroPadding for RawWireUnion {
    #[inline]
    fn zero_padding(_: &mut MaybeUninit<Self>) {
        // Wire unions have no padding
    }
}

impl RawWireUnion {
    /// Encodes that a union is absent in a slot.
    #[inline]
    pub fn encode_absent(out: &mut MaybeUninit<Self>) {
        munge!(let Self { ordinal, envelope } = out);

        ordinal.write(WireU64(0));
        WireEnvelope::encode_zero(envelope);
    }

    /// Encodes a `'static` value and ordinal in a slot.
    #[inline]
    pub fn encode_as_static<E: InternalHandleEncoder + ?Sized, T: Encode<E>>(
        value: T,
        ord: u64,
        encoder: &mut E,
        out: &mut MaybeUninit<Self>,
    ) -> Result<(), EncodeError> {
        munge!(let Self { ordinal, envelope } = out);

        ordinal.write(WireU64(ord));
        WireEnvelope::encode_value_static(value, encoder, envelope)
    }

    /// Encodes a value and ordinal in a slot.
    #[inline]
    pub fn encode_as<E: Encoder + ?Sized, T: Encode<E>>(
        value: T,
        ord: u64,
        encoder: &mut E,
        out: &mut MaybeUninit<Self>,
    ) -> Result<(), EncodeError> {
        munge!(let Self { ordinal, envelope } = out);

        ordinal.write(WireU64(ord));
        WireEnvelope::encode_value(value, encoder, envelope)
    }

    /// Returns the ordinal of the encoded value.
    #[inline]
    pub fn encoded_ordinal(slot: Slot<'_, Self>) -> u64 {
        munge!(let Self { ordinal, envelope: _ } = slot);
        **ordinal
    }

    /// Decodes an absent union from a slot.
    #[inline]
    pub fn decode_absent(slot: Slot<'_, Self>) -> Result<(), DecodeError> {
        munge!(let Self { ordinal: _, envelope } = slot);
        if !WireEnvelope::is_encoded_zero(envelope) {
            return Err(DecodeError::InvalidUnionEnvelope);
        }
        Ok(())
    }

    /// Decodes an unknown `'static` value from a union.
    ///
    /// The handles owned by the unknown value are discarded.
    #[inline]
    pub fn decode_unknown_static<D: InternalHandleDecoder + ?Sized>(
        slot: Slot<'_, Self>,
        decoder: &mut D,
    ) -> Result<(), DecodeError> {
        munge!(let Self { ordinal: _, envelope } = slot);
        WireEnvelope::decode_unknown_static(envelope, decoder)
    }

    /// Decodes an unknown value from a union.
    ///
    /// The handles owned by the unknown value are discarded.
    #[inline]
    pub fn decode_unknown<D: Decoder + ?Sized>(
        slot: Slot<'_, Self>,
        decoder: &mut D,
    ) -> Result<(), DecodeError> {
        munge!(let Self { ordinal: _, envelope } = slot);
        WireEnvelope::decode_unknown(envelope, decoder)
    }

    /// Decodes the typed `'static` value in a union.
    #[inline]
    pub fn decode_as_static<D: InternalHandleDecoder + ?Sized, T: Decode<D>>(
        slot: Slot<'_, Self>,
        decoder: &mut D,
    ) -> Result<(), DecodeError> {
        munge!(let Self { ordinal: _, envelope } = slot);
        WireEnvelope::decode_as_static::<D, T>(envelope, decoder)
    }

    /// Decodes the typed value in a union.
    #[inline]
    pub fn decode_as<D: Decoder + ?Sized, T: Decode<D>>(
        slot: Slot<'_, Self>,
        decoder: &mut D,
    ) -> Result<(), DecodeError> {
        munge!(let Self { ordinal: _, envelope } = slot);
        WireEnvelope::decode_as::<D, T>(envelope, decoder)
    }

    /// The absent optional union.
    #[inline]
    pub fn absent() -> Self {
        Self { ordinal: WireU64(0), envelope: WireEnvelope::zero() }
    }

    /// Returns whether the union contains a value.
    #[inline]
    pub fn is_some(&self) -> bool {
        *self.ordinal != 0
    }

    /// Returns whether the union is empty.
    #[inline]
    pub fn is_none(&self) -> bool {
        !self.is_some()
    }

    /// Returns the ordinal of the union.
    #[inline]
    pub fn ordinal(&self) -> u64 {
        *self.ordinal
    }

    /// Gets a reference to the envelope underlying the union.
    #[inline]
    pub fn get(&self) -> &WireEnvelope {
        &self.envelope
    }

    /// Clones the union, assuming that it contains an inline `T`.
    ///
    /// # Safety
    ///
    /// The union must have been successfully decoded as a `T`.
    #[inline]
    pub unsafe fn clone_unchecked<T: Clone>(&self) -> Self {
        Self { ordinal: self.ordinal, envelope: unsafe { self.envelope.clone_unchecked::<T>() } }
    }
}
