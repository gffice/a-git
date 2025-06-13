//! Module exposing structures relating to the reactor's view of a circuit's hops.

use super::CircuitCmd;
use super::{CloseStreamBehavior, SendRelayCell, SEND_WINDOW_INIT};
use crate::circuit::HopSettings;
use crate::congestion::sendme;
use crate::congestion::CongestionControl;
use crate::crypto::cell::HopNum;
use crate::stream::{AnyCmdChecker, StreamSendFlowControl, StreamStatus};
use crate::tunnel::circuit::{StreamMpscReceiver, StreamMpscSender};
use crate::tunnel::streammap::{
    self, EndSentStreamEnt, OpenStreamEnt, ShouldSendEnd, StreamEntMut,
};
use crate::tunnel::TunnelScopedCircId;
use crate::{Error, Result};

use futures::stream::FuturesUnordered;
use futures::Stream;
use safelog::sensitive as sv;
use tor_cell::chancell::BoxedCellBody;
use tor_cell::relaycell::msg::{AnyRelayMsg, Sendme};
use tor_cell::relaycell::{
    AnyRelayMsgOuter, RelayCellDecoder, RelayCellDecoderResult, RelayCellFormat, RelayCmd,
    RelayMsg, StreamId, UnparsedRelayMsg,
};

use tor_error::{internal, Bug};
use tracing::{trace, warn};

use std::pin::Pin;
use std::result::Result as StdResult;
use std::sync::{Arc, Mutex};
use std::task::Poll;

#[cfg(test)]
use tor_cell::relaycell::msg::SendmeTag;

/// Represents the reactor's view of a circuit's hop.
#[derive(Default)]
pub(crate) struct CircHopList {
    /// The list of hops.
    hops: Vec<CircHop>,
}

impl CircHopList {
    /// Return a reference to the hop corresponding to `hopnum`, if there is one.
    pub(super) fn hop(&self, hopnum: HopNum) -> Option<&CircHop> {
        self.hops.get(Into::<usize>::into(hopnum))
    }

    /// Return a mutable reference to the hop corresponding to `hopnum`, if there is one.
    pub(super) fn get_mut(&mut self, hopnum: HopNum) -> Option<&mut CircHop> {
        self.hops.get_mut(Into::<usize>::into(hopnum))
    }

    /// Append the specified hop.
    pub(crate) fn push(&mut self, hop: CircHop) {
        self.hops.push(hop);
    }

    /// Returns `true` if the list contains no [`CircHop`]s.
    pub(crate) fn is_empty(&self) -> bool {
        self.hops.is_empty()
    }

    /// Returns the number of hops in the list.
    pub(crate) fn len(&self) -> usize {
        self.hops.len()
    }

    /// Returns a [`Stream`] of [`CircuitCmd`] to poll from the main loop.
    ///
    /// The iterator contains at most one [`CircuitCmd`] for each hop,
    /// representing the instructions for handling the ready-item, if any,
    /// of its highest priority stream.
    ///
    /// IMPORTANT: this stream locks the stream map mutexes of each `CircHop`!
    /// To avoid contention, never create more than one
    /// [`ready_streams_iterator`](Self::ready_streams_iterator)
    /// stream at a time!
    ///
    /// This is cancellation-safe.
    pub(super) fn ready_streams_iterator(&self) -> impl Stream<Item = Result<CircuitCmd>> {
        self.hops
            .iter()
            .enumerate()
            .filter_map(|(i, hop)| {
                if !hop.ccontrol().can_send() {
                    // We can't send anything on this hop that counts towards SENDME windows.
                    //
                    // In theory we could send messages that don't count towards
                    // windows (like `RESOLVE`), and process end-of-stream
                    // events (to send an `END`), but it's probably not worth
                    // doing an O(N) iteration over flow-control-ready streams
                    // to see if that's the case.
                    //
                    // This *doesn't* block outgoing flow-control messages (e.g.
                    // SENDME), which are initiated via the control-message
                    // channel, handled above.
                    //
                    // TODO: Consider revisiting. OTOH some extra throttling when circuit-level
                    // congestion control has "bottomed out" might not be so bad, and the
                    // alternatives have complexity and/or performance costs.
                    return None;
                }

                let hop_num = HopNum::from(i as u8);
                let hop_map = Arc::clone(&self.hops[i].map);
                Some(futures::future::poll_fn(move |cx| {
                    // Process an outbound message from the first ready stream on
                    // this hop. The stream map implements round robin scheduling to
                    // ensure fairness across streams.
                    // TODO: Consider looping here to process multiple ready
                    // streams. Need to be careful though to balance that with
                    // continuing to service incoming and control messages.
                    let mut hop_map = hop_map.lock().expect("lock poisoned");
                    let Some((sid, msg)) = hop_map.poll_ready_streams_iter(cx).next() else {
                        // No ready streams for this hop.
                        return Poll::Pending;
                    };

                    if msg.is_none() {
                        return Poll::Ready(Ok(CircuitCmd::CloseStream {
                            hop: hop_num,
                            sid,
                            behav: CloseStreamBehavior::default(),
                            reason: streammap::TerminateReason::StreamTargetClosed,
                        }));
                    };
                    let msg = hop_map.take_ready_msg(sid).expect("msg disappeared");

                    #[allow(unused)] // unused in non-debug builds
                    let Some(StreamEntMut::Open(s)) = hop_map.get_mut(sid) else {
                        panic!("Stream {sid} disappeared");
                    };

                    debug_assert!(
                        s.can_send(&msg),
                        "Stream {sid} produced a message it can't send: {msg:?}"
                    );

                    let cell = SendRelayCell {
                        hop: hop_num,
                        early: false,
                        cell: AnyRelayMsgOuter::new(Some(sid), msg),
                    };
                    Poll::Ready(Ok(CircuitCmd::Send(cell)))
                }))
            })
            .collect::<FuturesUnordered<_>>()
    }

    /// Returns true if there are any streams on this circuit
    ///
    /// Important: this function locks the stream map of its each of the [`CircHop`]s
    /// in this circuit, so it must **not** be called from any function where the
    /// stream map lock is held (such as [`ready_streams_iterator`](Self::ready_streams_iterator).
    pub(super) fn has_streams(&self) -> bool {
        self.hops
            .iter()
            .any(|hop| hop.map.lock().expect("lock poisoned").n_open_streams() > 0)
    }

    /// Return the number of streams currently open on this circuit.
    pub(crate) fn n_open_streams(&self) -> usize {
        self.hops
            .iter()
            .map(|hop| hop.n_open_streams())
            // No need to worry about overflow; max streams per hop is U16_MAX
            .sum()
    }
}

/// Represents the reactor's view of a single hop.
pub(crate) struct CircHop {
    /// The unique ID of the circuit. Used for logging.
    unique_id: TunnelScopedCircId,
    /// Hop number in the path.
    hop_num: HopNum,
    /// Map from stream IDs to streams.
    ///
    /// We store this with the reactor instead of the circuit, since the
    /// reactor needs it for every incoming cell on a stream, whereas
    /// the circuit only needs it when allocating new streams.
    ///
    /// NOTE: this is behind a mutex because the reactor polls the `StreamMap`s
    /// of all hops concurrently, in a [`FuturesUnordered`]. Without the mutex,
    /// this wouldn't be possible, because it would mean holding multiple
    /// mutable references to `self` (the reactor). Note, however,
    /// that there should never be any contention on this mutex:
    /// we never create more than one
    /// [`ready_streams_iterator`](CircHopList::ready_streams_iterator) stream
    /// at a time, and we never clone/lock the hop's `StreamMap` outside of it.
    ///
    /// Additionally, the stream map of the last hop (join point) of a conflux tunnel
    /// is shared with all the circuits in the tunnel.
    map: Arc<Mutex<streammap::StreamMap>>,
    /// Congestion control object.
    ///
    /// This object is also in charge of handling circuit level SENDME logic for this hop.
    ccontrol: CongestionControl,
    /// Decodes relay cells received from this hop.
    inbound: RelayCellDecoder,
    /// Format to use for relay cells.
    //
    // When we have packed/fragmented cells, this may be replaced by a RelayCellEncoder.
    relay_format: RelayCellFormat,
}

impl CircHop {
    /// Create a new hop.
    pub(super) fn new(
        unique_id: TunnelScopedCircId,
        hop_num: HopNum,
        relay_format: RelayCellFormat,
        settings: &HopSettings,
    ) -> Self {
        CircHop {
            unique_id,
            hop_num,
            map: Arc::new(Mutex::new(streammap::StreamMap::new())),
            ccontrol: CongestionControl::new(&settings.ccontrol),
            inbound: RelayCellDecoder::new(relay_format),
            relay_format,
        }
    }

    /// Start a stream. Creates an entry in the stream map with the given channels, and sends the
    /// `message` to the provided hop.
    pub(crate) fn begin_stream(
        &mut self,
        message: AnyRelayMsg,
        sender: StreamMpscSender<UnparsedRelayMsg>,
        rx: StreamMpscReceiver<AnyRelayMsg>,
        cmd_checker: AnyCmdChecker,
    ) -> Result<(SendRelayCell, StreamId)> {
        let flow_ctrl = self.build_send_flow_ctrl();
        let r =
            self.map
                .lock()
                .expect("lock poisoned")
                .add_ent(sender, rx, flow_ctrl, cmd_checker)?;
        let cell = AnyRelayMsgOuter::new(Some(r), message);
        Ok((
            SendRelayCell {
                hop: self.hop_num,
                early: false,
                cell,
            },
            r,
        ))
    }

    /// Close the stream associated with `id` because the stream was
    /// dropped.
    ///
    /// If we have not already received an END cell on this stream, send one.
    /// If no END cell is specified, an END cell with the reason byte set to
    /// REASON_MISC will be sent.
    pub(super) fn close_stream(
        &mut self,
        id: StreamId,
        message: CloseStreamBehavior,
        why: streammap::TerminateReason,
    ) -> Result<Option<SendRelayCell>> {
        let should_send_end = self.map.lock().expect("lock poisoned").terminate(id, why)?;
        trace!(
            circ_id = %self.unique_id,
            stream_id = %id,
            should_send_end = ?should_send_end,
            "Ending stream",
        );
        // TODO: I am about 80% sure that we only send an END cell if
        // we didn't already get an END cell.  But I should double-check!
        if let (ShouldSendEnd::Send, CloseStreamBehavior::SendEnd(end_message)) =
            (should_send_end, message)
        {
            let end_cell = AnyRelayMsgOuter::new(Some(id), end_message.into());
            let cell = SendRelayCell {
                hop: self.hop_num,
                early: false,
                cell: end_cell,
            };

            return Ok(Some(cell));
        }
        Ok(None)
    }

    /// Return the format that is used for relay cells sent to this hop.
    ///
    /// For the most part, this format isn't necessary to interact with a CircHop;
    /// it becomes relevant when we are deciding _what_ we can encode for the hop.
    pub(crate) fn relay_cell_format(&self) -> RelayCellFormat {
        self.relay_format
    }

    /// Delegate to CongestionControl, for testing purposes
    #[cfg(test)]
    pub(crate) fn send_window_and_expected_tags(&self) -> (u32, Vec<SendmeTag>) {
        self.ccontrol.send_window_and_expected_tags()
    }

    /// Return the number of open streams on this hop.
    ///
    /// WARNING: because this locks the stream map mutex,
    /// it should never be called from a context where that mutex is already locked.
    pub(crate) fn n_open_streams(&self) -> usize {
        self.map.lock().expect("lock poisoned").n_open_streams()
    }

    /// Return a reference to our CongestionControl object.
    pub(crate) fn ccontrol(&self) -> &CongestionControl {
        &self.ccontrol
    }

    /// Return a mutable reference to our CongestionControl object.
    pub(crate) fn ccontrol_mut(&mut self) -> &mut CongestionControl {
        &mut self.ccontrol
    }

    /// Return the RelayCellFormat.
    pub(crate) fn relay_format(&self) -> RelayCellFormat {
        self.relay_format
    }

    /// Take capacity to send `msg`.
    ///
    /// See [`OpenStreamEnt::take_capacity_to_send`].
    pub(crate) fn take_capacity_to_send<M: RelayMsg>(
        &mut self,
        stream_id: StreamId,
        msg: &M,
    ) -> Result<()> {
        let mut hop_map = self.map.lock().expect("lock poisoned");
        let Some(StreamEntMut::Open(ent)) = hop_map.get_mut(stream_id) else {
            warn!(
                circ_id = %self.unique_id,
                stream_id = %stream_id,
                "sending a relay cell for non-existent or non-open stream!",
            );
            return Err(Error::CircProto(format!(
                "tried to send a relay cell on non-open stream {}",
                sv(stream_id),
            )));
        };

        ent.take_capacity_to_send(msg)
    }

    /// Add an entry to this map using the specified StreamId.
    #[cfg(feature = "hs-service")]
    pub(super) fn add_ent_with_id(
        &self,
        sink: StreamMpscSender<UnparsedRelayMsg>,
        rx: StreamMpscReceiver<AnyRelayMsg>,
        stream_id: StreamId,
        cmd_checker: AnyCmdChecker,
    ) -> Result<()> {
        let mut hop_map = self.map.lock().expect("lock poisoned");
        hop_map.add_ent_with_id(
            sink,
            rx,
            self.build_send_flow_ctrl(),
            stream_id,
            cmd_checker,
        )?;

        Ok(())
    }

    /// Note that we received an END message (or other message indicating the end of
    /// the stream) on the stream with `id`.
    ///
    /// See [`StreamMap::ending_msg_received`](super::streammap::StreamMap::ending_msg_received).
    #[cfg(feature = "hs-service")]
    pub(super) fn ending_msg_received(&self, stream_id: StreamId) -> Result<()> {
        let mut hop_map = self.map.lock().expect("lock poisoned");

        hop_map.ending_msg_received(stream_id)?;

        Ok(())
    }

    /// Parse a RELAY or RELAY_EARLY cell body.
    ///
    /// Requires that the cryptographic checks on the message have already been
    /// performed
    pub(super) fn decode(&mut self, cell: BoxedCellBody) -> Result<RelayCellDecoderResult> {
        self.inbound
            .decode(cell)
            .map_err(|e| Error::from_bytes_err(e, "relay cell"))
    }

    /// Handle `msg`, delivering it to the stream with the specified `streamid` if appropriate.
    ///
    /// Returns back the provided `msg`, if the message is an incoming stream request
    /// that needs to be handled by the calling code.
    ///
    // TODO: the above is a bit of a code smell -- we should try to avoid passing the msg
    // back and forth like this.
    pub(super) fn handle_msg(
        &self,
        cell_counts_toward_windows: bool,
        streamid: StreamId,
        msg: UnparsedRelayMsg,
    ) -> Result<Option<UnparsedRelayMsg>> {
        let mut hop_map = self.map.lock().expect("lock poisoned");
        match hop_map.get_mut(streamid) {
            Some(StreamEntMut::Open(ent)) => {
                // Can't have a stream level SENDME when congestion control is enabled.
                let message_closes_stream =
                    Self::deliver_msg_to_stream(streamid, ent, cell_counts_toward_windows, msg)?;

                if message_closes_stream {
                    hop_map.ending_msg_received(streamid)?;
                }
            }
            #[cfg(feature = "hs-service")]
            Some(StreamEntMut::EndSent(_))
                if matches!(
                    msg.cmd(),
                    RelayCmd::BEGIN | RelayCmd::BEGIN_DIR | RelayCmd::RESOLVE
                ) =>
            {
                // If the other side is sending us a BEGIN but hasn't yet acknowledged our END
                // message, just remove the old stream from the map and stop waiting for a
                // response
                hop_map.ending_msg_received(streamid)?;
                return Ok(Some(msg));
            }
            Some(StreamEntMut::EndSent(EndSentStreamEnt { half_stream, .. })) => {
                // We sent an end but maybe the other side hasn't heard.

                match half_stream.handle_msg(msg)? {
                    StreamStatus::Open => {}
                    StreamStatus::Closed => {
                        hop_map.ending_msg_received(streamid)?;
                    }
                }
            }
            #[cfg(feature = "hs-service")]
            None if matches!(
                msg.cmd(),
                RelayCmd::BEGIN | RelayCmd::BEGIN_DIR | RelayCmd::RESOLVE
            ) =>
            {
                return Ok(Some(msg));
            }
            _ => {
                // No stream wants this message, or ever did.
                return Err(Error::CircProto(
                    "Cell received on nonexistent stream!?".into(),
                ));
            }
        }

        Ok(None)
    }

    /// Builds the (sending) flow control handler for a new stream.
    fn build_send_flow_ctrl(&self) -> StreamSendFlowControl {
        if self.ccontrol.uses_stream_sendme() {
            let window = sendme::StreamSendWindow::new(SEND_WINDOW_INIT);
            StreamSendFlowControl::new_window_based(window)
        } else {
            StreamSendFlowControl::new_xon_xoff_based()
        }
    }

    /// Deliver `msg` to the specified open stream entry `ent`.
    fn deliver_msg_to_stream(
        streamid: StreamId,
        ent: &mut OpenStreamEnt,
        cell_counts_toward_windows: bool,
        msg: UnparsedRelayMsg,
    ) -> Result<bool> {
        use tor_async_utils::SinkTrySend as _;
        use tor_async_utils::SinkTrySendError as _;

        // The stream for this message exists, and is open.

        if msg.cmd() == RelayCmd::SENDME {
            let _sendme = msg
                .decode::<Sendme>()
                .map_err(|e| Error::from_bytes_err(e, "Sendme message on stream"))?
                .into_msg();

            // We need to handle sendmes here, not in the stream's
            // recv() method, or else we'd never notice them if the
            // stream isn't reading.
            ent.put_for_incoming_sendme()?;
            return Ok(false);
        }

        let message_closes_stream = ent.cmd_checker.check_msg(&msg)? == StreamStatus::Closed;

        if let Err(e) = Pin::new(&mut ent.sink).try_send(msg) {
            if e.is_full() {
                // If we get here, we either have a logic bug (!), or an attacker
                // is sending us more cells than we asked for via congestion control.
                return Err(Error::CircProto(format!(
                    "Stream sink would block; received too many cells on stream ID {}",
                    sv(streamid),
                )));
            }
            if e.is_disconnected() && cell_counts_toward_windows {
                // the other side of the stream has gone away; remember
                // that we received a cell that we couldn't queue for it.
                //
                // Later this value will be recorded in a half-stream.
                ent.dropped += 1;
            }
        }

        Ok(message_closes_stream)
    }

    /// Get the stream map of this hop.
    pub(crate) fn stream_map(&self) -> &Arc<Mutex<streammap::StreamMap>> {
        &self.map
    }

    /// Set the stream map of this hop to `map`.
    ///
    /// Returns an error if the existing stream map of the hop has any open stream.
    pub(crate) fn set_stream_map(
        &mut self,
        map: Arc<Mutex<streammap::StreamMap>>,
    ) -> StdResult<(), Bug> {
        if self.n_open_streams() != 0 {
            return Err(internal!("Tried to discard existing open streams?!"));
        }

        self.map = map;

        Ok(())
    }
}
