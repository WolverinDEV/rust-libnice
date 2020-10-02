//! See `test::connects_and_transmits_data` for a usage example.
use crate::ffi;
use futures::channel::mpsc;
use futures::executor::block_on;
use futures::io::{AsyncRead, AsyncWrite};
use futures::pin_mut;
use futures::ready;
use futures::sink::SinkExt;
use futures::task::Poll;
use futures::Sink;
use futures::Stream as FuturesStream;
use glib::MainContext;
use std::collections::HashMap;
use std::ffi::CString;
use std::future::Future;
use std::io;
use std::io::Read;
use std::ops::DerefMut;
use std::os::raw::c_uint;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::Mutex;
use std::task::Context;

pub use crate::ffi::BoolResult;
pub use crate::ffi::NiceCompatibility;
pub use crate::ffi::NiceComponentState as ComponentState;
pub use webrtc_sdp::attribute_type::SdpAttributeCandidate as Candidate;
use crate::ffi::NiceComponentState;
use libnice_sys::NiceAgentOption;

type ComponentId = (c_uint, c_uint);

/// A single, high-level ICE agent.
///
/// **Note**: The agent implements [Future] and needs to be [`poll()`ed] for any of its [Stream]s
///           to make progress.
///
/// [`poll()`ed]: Future::poll
pub struct Agent {
    ctx: MainContext,
    agent: ffi::NiceAgent,
    msgs_sender: mpsc::UnboundedSender<ControlMsg>,
    msgs: mpsc::UnboundedReceiver<ControlMsg>,

    candidate_sinks: Arc<Mutex<HashMap<c_uint, mpsc::UnboundedSender<Candidate>>>>,
    state_sinks: Arc<Mutex<HashMap<ComponentId, mpsc::Sender<ComponentState>>>>,
}

impl Agent {
    /// Creates a new ICE agent in RFC5245 (ICE) compatibility mode.
    pub fn new_rfc5245(context: MainContext) -> Self {
        Self::new(context, NiceCompatibility::RFC5245)
    }

    /// Creates a new ICE agent with the specified compatibility mode.
    pub fn new(ctx: MainContext, compat: NiceCompatibility) -> Self {
        let agent = ffi::NiceAgent::new(&ctx, compat);
        Self::construct(ctx, agent)
    }

    /// Creates a new ICE agent with the specified compatibility mode and agent options
    pub fn new_full(ctx: MainContext, compat: NiceCompatibility, flags: NiceAgentOption) -> Self {
        let agent = ffi::NiceAgent::new_full(&ctx, compat, flags);
        Self::construct(ctx, agent)
    }

    fn construct(ctx: MainContext, mut agent: ffi::NiceAgent) -> Self {
        // Channel for sending messages from streams to the agent
        let (msgs_sender, msgs) = mpsc::unbounded();

        // Channel for sending candidates to streams
        let candidate_sinks: Arc<Mutex<HashMap<c_uint, mpsc::UnboundedSender<Candidate>>>> = Default::default();
        let candidate_sinks_clone = Arc::clone(&candidate_sinks);
        agent
            .on_new_candidate(move |candidate| {
                let mut candidate_sinks = candidate_sinks_clone.lock().unwrap();
                let stream_id = &candidate.stream_id();
                let sink = candidate_sinks.get_mut(stream_id).expect(format!("received candidate for stream {} but it does not exists", stream_id).as_str());
                if sink.unbounded_send(candidate.to_sdp()).is_err() {
                    candidate_sinks.remove(stream_id);
                }
            })
            .unwrap();
        let candidate_sinks_clone = Arc::clone(&candidate_sinks);
        agent
            .on_candidate_gathering_done(move |stream_id| {
                /* TODO: Send a candidate gathering done event */
                let mut candidate_sinks = candidate_sinks_clone.lock().unwrap();
                candidate_sinks.remove(&stream_id).expect(format!("received candidate gathering done signal for stream {} but it does not exists", stream_id).as_str());
            })
            .unwrap();

        // Channel for sending state updates to components
        let state_sinks: Arc<Mutex<HashMap<ComponentId, mpsc::Sender<ComponentState>>>> =
            Default::default();
        let state_sinks_clone = Arc::clone(&state_sinks);
        agent
            .on_component_state_changed(move |stream_id, component_id, new_state| {
                let mut state_sinks = state_sinks_clone.lock().unwrap();
                let key = (stream_id, component_id);
                let sink = state_sinks.get_mut(&key).expect(format!("received state change for stream {}.{} but it does not exists", stream_id, component_id).as_str());
                if block_on(sink.send(new_state)).is_err() {
                    state_sinks.remove(&key);
                }
            })
            .unwrap();

        Agent {
            ctx,
            agent,
            msgs_sender,
            msgs,
            candidate_sinks,
            state_sinks
        }
    }

    /// Returns the context this agent is running on.
    pub fn get_ctx(&self) -> &MainContext {
        &self.ctx
    }

    /// Returns the low-level agent backing this Agent.
    pub fn get_ffi_agent(&mut self) -> &mut ffi::NiceAgent {
        &mut self.agent
    }

    /// See the [libnice] documentation for more info.
    ///
    /// [libnice]: https://nice.freedesktop.org/libnice/NiceAgent.html#nice-agent-set-software
    pub fn set_software(&mut self, name: impl Into<String>) {
        let name = CString::new(name.into()).expect("name must not have have null bytes");
        self.agent.set_software(name);
    }

    /// Changes whether this agent is in controlling mode (by default it is not).
    pub fn set_controlling_mode(&mut self, controlling: bool) {
        self.agent.set_controlling_mode(controlling);
    }

    /// Add a new [Stream] with the specified amount of components to the agent.
    pub fn stream_builder(&mut self, components: usize) -> StreamBuilder {
        StreamBuilder::new(self, components)
    }

    fn handle_msg(&mut self, msg: ControlMsg) {
        match msg {
            ControlMsg::SetRemoteCredentials(stream_id, ufrag, pwd) => {
                let _ = self.agent.set_remote_credentials(stream_id, &ufrag, &pwd);
            }
            ControlMsg::AddRemoteCandidate((stream_id, component_id), candidate) => {
                // TODO resolve FQDN in candidate (if any)
                let candidate = match ffi::NiceCandidate::from_sdp_without_fqdn(&candidate) {
                    Ok(candidate) => candidate,
                    Err(_) => return, // rfc mandates we MUST ignore unsupported lines
                };
                let candidate_ref = &candidate;
                let candidates = std::slice::from_ref(&candidate_ref);
                let _ = self
                    .agent
                    .add_remote_candidates(stream_id, component_id, candidates);
            }
            ControlMsg::Send((stream_id, component_id), buf) => {
                // The libnice docs are very unclear on when this can fail with unreliable
                // transports, so we'll just assume it only fails for WOULD_BLOCK.
                let _ = self.agent.send(stream_id, component_id, &buf);
            }
            ControlMsg::DropStream(stream_id) => {
                self.remove_stream_internal(stream_id);
            }
        }
    }

    /// Removes a stream from the nice agent.
    /// This steam must not be registered at this agent.
    fn remove_stream_internal(&mut self, stream_id: u32) {
        println!("Removing stream {}", stream_id);

        /*
         * Just get all known components.
         * We drain them already since events could still be fired right now.
         */
        let components = self.state_sinks
            .lock().unwrap()
            .iter()
            .map(|e| *e.0)
            .filter(|e| e.0 == stream_id)
            .collect::<Vec<ComponentId>>();

        for component in components.iter() {
            let _ = self.agent.detach_recv(stream_id, component.0, &self.ctx);
        }

        self.agent.remove_stream(stream_id);

        let mut state_sinks = self.state_sinks.lock().unwrap();
        for key in components {
            state_sinks.remove(&key);
        }

        self.candidate_sinks.lock().unwrap().remove(&stream_id);
    }
}

/*
 * The nice wrapper itself is safe to use across threads
 */
unsafe impl Sync for Agent {}

/* TODO: Is this really true? */
unsafe impl Send for Agent {}

impl Future for Agent {
    type Output = (); // never

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context) -> Poll<Self::Output> {
        loop {
            let msg = {
                let msgs = &mut self.msgs;
                pin_mut!(msgs);
                ready!(msgs.poll_next(cx)).expect("msgs stream ended prematurely")
            };
            self.handle_msg(msg);
        }
    }
}

impl Drop for Agent {
    fn drop(&mut self) {
        for (_, sink) in self.state_sinks.lock().expect("failed to lock stream state sinks").iter_mut() {
            let _ = sink.send(NiceComponentState::Disconnected);
            sink.close_channel()
        }
    }
}

/// Builder for ICE [Stream]s.
pub struct StreamBuilder<'a> {
    agent: &'a mut Agent,
    components: usize,
    inbound_buf_size: usize,
    port_ranges: HashMap<usize, (u16, u16)>,
}

impl<'a> StreamBuilder<'a> {
    /// See [Agent::stream_builder].
    pub fn new(agent: &'a mut Agent, components: usize) -> Self {
        Self {
            agent,
            components,
            inbound_buf_size: 10,
            port_ranges: HashMap::new(),
        }
    }

    /// Sets the size of the buffer used to store inbound packets.
    pub fn set_inbound_buffer_size(&mut self, size: usize) -> &mut Self {
        self.inbound_buf_size = size;
        self
    }

    /// Limits the range of ports used for host candidates.
    ///
    /// If the range is exhausted, [StreamBuilder::build] will fail.
    /// To set the range per component, use [StreamBuilder::set_component_port_range].
    pub fn set_port_range(&mut self, min_port: u16, max_port: u16) -> &mut Self {
        for i in 0..self.components {
            self.port_ranges.insert(i, (min_port, max_port));
        }
        self
    }

    /// Limits the range of ports used for host candidates for the component at the specified index.
    /// Note that the first component (with id `1`) is at index `0`.
    ///
    /// If the range is exhausted, [StreamBuilder::build] will fail.
    /// To set the range for all components, use [StreamBuilder::set_port_range].
    ///
    /// # Panics
    ///
    /// Panics if `component_index >= components`.
    pub fn set_component_port_range(
        &mut self,
        component_index: usize,
        min_port: u16,
        max_port: u16,
    ) -> &mut Self {
        if component_index >= self.components {
            panic!(
                "index {} of of range (size: {})",
                component_index, self.components
            );
        }
        self.port_ranges
            .insert(component_index, (min_port, max_port));
        self
    }

    /// Build the [Stream].
    pub fn build(&mut self) -> BoolResult<Stream> {
        let stream_id = self.agent.agent.add_stream(self.components as c_uint)?;

        match self.configure_stream(stream_id) {
            Ok(stream) => Ok(stream),
            Err(error) => {
                self.agent.remove_stream_internal(stream_id);
                Err(error)
            }
        }
    }

    fn configure_stream(&mut self, stream_id: u32) -> BoolResult<Stream> {
        let agent = &mut self.agent;
        let ffi = &mut agent.agent;

        let (local_ufrag, local_pwd) = ffi.get_local_credentials(stream_id).expect("local credentials");
        let local_ufrag = local_ufrag
            .into_string()
            .expect("generated ufrag is valid utf8");
        let local_pwd = local_pwd
            .into_string()
            .expect("generated pwd is valid utf8");

        let mut components = Vec::new();
        for i in 0..(self.components as c_uint) {
            let component_id = i + 1;
            let (mut source_sender, source) = mpsc::channel(self.inbound_buf_size);
            let recv_handle = ffi.attach_recv(stream_id, component_id, &agent.ctx, move |buf| {
                let _ = source_sender.try_send(buf.to_vec());
            })?;

            let (state_sender, state_stream) = mpsc::channel(8);
            agent.state_sinks.lock().unwrap().insert((stream_id, component_id), state_sender);

            components.push(StreamComponent {
                _recv_handle: recv_handle,
                stream_id,
                component_id,
                state: ComponentState::Disconnected,
                state_stream,
                source,
                sink: agent.msgs_sender.clone(),
            });
        }

        for (index, (min_port, max_port)) in &self.port_ranges {
            ffi.set_port_range(stream_id, *index as c_uint + 1, *min_port, *max_port);
        }

        let (candidate_sink, candidates) = mpsc::unbounded();
        agent.candidate_sinks.lock().unwrap().insert(stream_id, candidate_sink);

        /* this call will already trigger some candidate found events */
        ffi.gather_candidates(stream_id)?;

        Ok(Stream {
            id: stream_id,
            component_count: self.components,
            local_ufrag,
            local_pwd,
            msg_sink: agent.msgs_sender.clone(),
            candidates,
            components,
        })
    }
}

enum ControlMsg {
    SetRemoteCredentials(c_uint, CString, CString),
    AddRemoteCandidate(ComponentId, Candidate),
    Send(ComponentId, Vec<u8>),
    DropStream(c_uint)
}

/// An ICE stream consisting of multiple components.
///
/// Implements [futures::Stream] which emits the local ICE candidates for this stream as they are
/// being discovered.
///
/// Attention: This stream must be kept alive while using any of the components.
///            If not done, the stream and the components will be unregistered
pub struct Stream {
    id: c_uint,
    component_count: usize,
    local_ufrag: String,
    local_pwd: String,
    msg_sink: mpsc::UnboundedSender<ControlMsg>,
    candidates: mpsc::UnboundedReceiver<Candidate>,
    components: Vec<StreamComponent>,
}

impl Stream {
    /// See [Agent::stream_builder].
    pub fn builder(agent: &mut Agent, components: usize) -> StreamBuilder {
        StreamBuilder::new(agent, components)
    }

    /// Returns the local STUN ufrag for this stream.
    pub fn get_local_ufrag(&self) -> &str {
        &self.local_ufrag
    }

    /// Returns the local STUN pwd for this stream.
    pub fn get_local_pwd(&self) -> &str {
        &self.local_pwd
    }

    /// Set the remote STUN credentials for this stream.
    pub fn set_remote_credentials(&mut self, ufrag: CString, pwd: CString) {
        let msg = ControlMsg::SetRemoteCredentials(self.id, ufrag, pwd);
        let _ = self.msg_sink.unbounded_send(msg);
    }

    /// Adds a new remote ICE candidate for this stream.
    pub fn add_remote_candidate(&mut self, candidate: Candidate) {
        assert!(candidate.component > 0);
        assert!((candidate.component as usize) <= self.component_count);
        let msg = ControlMsg::AddRemoteCandidate((self.id, candidate.component), candidate);
        let _ = self.msg_sink.unbounded_send(msg);
    }

    /// Returns a references to the components of this stream.
    pub fn components(&self) -> &[StreamComponent] {
        &self.components
    }

    /// Returns a mutable references to the components of this stream.
    pub fn mut_components(&mut self) -> &mut [StreamComponent] {
        &mut self.components
    }

    /// Returns the components of this stream, returning an empty Vec on subsequent calls.
    pub fn take_components(&mut self) -> Vec<StreamComponent> {
        std::mem::replace(&mut self.components, Vec::new())
    }

    /*
    /// Returns the components of this stream, consuming the stream.
    ///
    /// Note that this should probably only be called after all ICE candidates have been exchanged.
    /// Until then, use [Stream::mut_components] or [Stream::take_components] instead.
    pub fn into_components(self) -> Vec<StreamComponent> {
        self.components
    }
    */
}

impl FuturesStream for Stream {
    type Item = Candidate;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context) -> Poll<Option<Self::Item>> {
        let f = &mut self.candidates;
        pin_mut!(f);
        f.poll_next(cx)
    }
}

impl Drop for Stream {
    fn drop(&mut self) {
        let _ = self.msg_sink.unbounded_send(ControlMsg::DropStream(self.id));
    }
}

/// A single ICE stream component.
/// It implements [Stream]+[Sink] as well as [AsyncRead]+[AsyncWrite].
pub struct StreamComponent {
    _recv_handle: ffi::AttachRecvHandle,
    stream_id: c_uint,
    component_id: c_uint,
    state: ComponentState,
    state_stream: mpsc::Receiver<ComponentState>,
    source: mpsc::Receiver<Vec<u8>>,
    sink: mpsc::UnboundedSender<ControlMsg>,
}

impl StreamComponent {
    /// Adds a remote ICE candidate to this stream component.
    pub fn add_remote_candidate(&mut self, candidate: Candidate) {
        let msg = ControlMsg::AddRemoteCandidate((self.stream_id, self.component_id), candidate);
        let _ = self.sink.unbounded_send(msg);
    }

    /// Sends a packet of data via this component.
    ///
    /// Note that the [Agent] needs to be `poll()`ed for sending to make progress.
    pub fn unbounded_send(&mut self, item: Vec<u8>) {
        let msg = ControlMsg::Send((self.stream_id, self.component_id), item);
        let _ = self.sink.unbounded_send(msg);
    }

    /// Returns the current state of this component.
    ///
    /// Note that the returned state only reflects the state of this stream at the last time it
    /// was `poll()`ed by reading or [StreamComponent::wait_for_state].
    pub fn get_state(&self) -> ComponentState {
        self.state
    }

    /// Returns a future which waits until the component is in the target state or has surpassed
    /// the target state (e.g. waiting for Connected will also be done when the state is Ready).
    ///
    /// The returned future will fail if the agent or the stream is closed or the component
    /// switches to Failed state.
    pub fn wait_for_state(self, target: ComponentState) -> ComponentStateFuture {
        ComponentStateFuture {
            component: Some(self),
            target,
        }
    }

    /// Updates the current state by polling [state_stream].
    /// Returns `Poll::Ready(())` when [state_stream] has been closed.
    pub fn poll_state(&mut self, cx: &mut Context) -> Poll<()> {
        loop {
            let state_stream = &mut self.state_stream;
            pin_mut!(state_stream);
            match state_stream.poll_next(cx) {
                Poll::Pending => return Poll::Pending,
                Poll::Ready(Some(new_state)) => {
                    self.state = new_state;
                }
                Poll::Ready(None) => return Poll::Ready(()),
            }
        }
    }
}

/// Future returned by [StreamComponent::wait_for_state]
pub struct ComponentStateFuture {
    component: Option<StreamComponent>,
    target: ComponentState,
}

impl Future for ComponentStateFuture {
    type Output = Option<StreamComponent>; // none if stream (or agent) has been closed

    fn poll(self: Pin<&mut Self>, cx: &mut Context) -> Poll<Self::Output> {
        fn rate(state: ComponentState) -> u8 {
            match state {
                ComponentState::Disconnected => 0,
                ComponentState::Gathering => 1,
                ComponentState::Connecting => 2,
                ComponentState::Connected => 3,
                ComponentState::Ready => 4,
                ComponentState::Failed => 5,
            }
        }
        let this = self.get_mut();
        let component = this.component.as_mut().expect("poll called after Ready");
        if let Poll::Ready(()) = component.poll_state(cx) {
            return Poll::Ready(None);
        }
        if rate(component.state) >= rate(this.target) {
            if component.state == ComponentState::Failed {
                Poll::Ready(None)
            } else {
                Poll::Ready(Some(this.component.take().unwrap()))
            }
        } else {
            Poll::Pending
        }
    }
}

impl FuturesStream for StreamComponent {
    type Item = Vec<u8>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context) -> Poll<Option<Self::Item>> {
        if let Poll::Ready(()) = self.deref_mut().poll_state(cx) {
            return Poll::Ready(None);
        }
        let source = &mut self.source;
        pin_mut!(source);
        source.poll_next(cx)
    }
}

impl Sink<Vec<u8>> for StreamComponent {
    type Error = (); // never

    fn poll_ready(self: Pin<&mut Self>, _cx: &mut Context) -> Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }

    fn start_send(self: Pin<&mut Self>, item: Vec<u8>) -> Result<(), Self::Error> {
        let msg = ControlMsg::Send((self.stream_id, self.component_id), item);
        let _ = self.sink.unbounded_send(msg);
        Ok(())
    }

    fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context) -> Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }

    fn poll_close(self: Pin<&mut Self>, _cx: &mut Context) -> Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }
}

impl AsyncRead for StreamComponent {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context,
        buf: &mut [u8],
    ) -> Poll<Result<usize, io::Error>> {
        match self.poll_next(cx) {
            Poll::Ready(Some(vec)) => Poll::Ready(vec.as_slice().read(buf)),
            Poll::Ready(None) => Poll::Ready(Ok(0)),
            Poll::Pending => Poll::Pending,
        }
    }
}

impl AsyncWrite for StreamComponent {
    fn poll_write(
        self: Pin<&mut Self>,
        _cx: &mut Context,
        buf: &[u8],
    ) -> Poll<Result<usize, io::Error>> {
        let _ = self.start_send(buf.to_vec());
        Poll::Ready(Ok(buf.len()))
    }

    fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context) -> Poll<Result<(), io::Error>> {
        Poll::Ready(Ok(()))
    }

    fn poll_close(self: Pin<&mut Self>, _cx: &mut Context) -> Poll<Result<(), io::Error>> {
        Poll::Ready(Ok(()))
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use futures::StreamExt;
    use tokio::runtime;
    use glib::MainLoop;
    use glib::translate::ToGlibPtr;
    use std::ffi::CStr;

    #[test]
    fn connects_and_transmits_data() {
        #[cfg(target_os = "windows")]
            unsafe {
            let mut wsa_data: winapi::um::winsock2::WSADATA = std::mem::MaybeUninit::uninit().assume_init();
            let result = winapi::um::winsock2::WSAStartup(0x202, &mut wsa_data);
            if result != 0 {
                panic!("WSAStartup failed with code {:?}", result);
            }

            println!("WSAInfo::szDescription = {:?}", CStr::from_ptr(&mut wsa_data.szDescription[0]));
            println!("WSAInfo::szSystemStatus = {:?}", CStr::from_ptr(&mut wsa_data.szSystemStatus[0]));
            println!("WSAInfo::lpVendorInfo = {:?}", wsa_data.lpVendorInfo);
        }

        let mut executor = runtime::Builder::new().basic_scheduler().build().unwrap();

        let ctx = MainContext::new();
        let main_loop = MainLoop::new(Some(&ctx), false);

        // Start main loop on new thread
        let main_loop_clone = main_loop.clone();
        std::thread::spawn(move || {
            if !main_loop_clone.get_context().acquire() {
                panic!("failed to acquire main loop");
            }
            main_loop_clone.run();
        });

        println!("Creating server/client");
        // Create ICE agents
        let mut server = Agent::new_rfc5245(main_loop.get_context());
        let mut client = Agent::new_rfc5245(main_loop.get_context());
        client.set_controlling_mode(true);

        unsafe {
            let mut address: libnice_sys::NiceAddress = std::mem::MaybeUninit::uninit().assume_init();
            libnice_sys::nice_address_set_ipv4(&mut address, 0x7F000001);
            libnice_sys::nice_agent_add_local_address(server.agent.to_glib_none().0, &mut address);
            libnice_sys::nice_agent_add_local_address(client.agent.to_glib_none().0, &mut address);
        }

        println!("Starting server/client");
        // Create one ICE stream per agent, each with one component
        let mut server_stream = server.stream_builder(2).build().unwrap();
        let mut client_stream = client.stream_builder(2).build().unwrap();
        println!("Started server/client");

        // Exchange ICE credentials
        server_stream.set_remote_credentials(
            CString::new(client_stream.get_local_ufrag()).unwrap(),
            CString::new(client_stream.get_local_pwd()).unwrap(),
        );
        client_stream.set_remote_credentials(
            CString::new(server_stream.get_local_ufrag()).unwrap(),
            CString::new(server_stream.get_local_pwd()).unwrap(),
        );

        // Poll agents to make connection (and candidate-gathering) progress
        // Note: Normally you'd want some way to drop the agent once you no longer need it,
        //       here we just drop the executor once we're done.
        executor.spawn(server);
        executor.spawn(client);

        // Exchange ICE candidates
        // Note that the connection might already start working before all have been exchanged
        // but continuing might improve the network path taken and provide fallback options.
        for candidate in executor.block_on(server_stream.by_ref().collect::<Vec<Candidate>>()) {
            println!("Server candidate: {}", candidate.to_string());
            client_stream.add_remote_candidate(candidate);
        }
        for candidate in executor.block_on(client_stream.by_ref().collect::<Vec<Candidate>>()) {
            println!("Client candidate: {}", candidate.to_string());
            server_stream.add_remote_candidate(candidate);
        }

        // Grab components for later use (you could also ship them off to different tasks here)
        let mut server_components = server_stream.take_components();
        let mut client_components = client_stream.take_components();

        while !server_components.is_empty() {
            // Wait until the component state reaches Connected, otherwise data will just be dropped
            let mut server_component = executor
                .block_on(server_components.pop().unwrap().wait_for_state(ComponentState::Connected))
                .unwrap();
            let mut client_component = executor
                .block_on(client_components.pop().unwrap().wait_for_state(ComponentState::Connected))
                .unwrap();

            // Send some data (potentially unreliable, hence unbounded)
            server_component.unbounded_send(vec![1, 2, 3, 4, server_component.component_id as u8]);
            client_component.unbounded_send(vec![42, client_component.component_id as u8]);

            // Check that we received it
            // Note that we can be fairly sure here (local-to-local) but under normal circumstances
            // the transport must be assumed to be unreliable!
            assert_eq!(
                Some(vec![42, client_component.component_id as u8]),
                executor.block_on(server_component.by_ref().into_future()).0
            );
            assert_eq!(
                Some(vec![1, 2, 3, 4, server_component.component_id as u8]),
                executor.block_on(client_component.by_ref().into_future()).0
            );
        }
    }
}
