use std::mem;
use std::thread::{Builder, JoinHandle};
use std::io::{stderr, Write};
use std::ffi::{CString, CStr};

use alsa::{Seq, Direction};
use alsa::seq::{PortInfo, PortSubscribe, Addr, QueueTempo, EventType, MIDI_GENERIC, APPLICATION, WRITE, SUBS_WRITE, READ, SUBS_READ};

use ::{MidiMessage, Ignore};
use ::errors::*;

mod helpers {
    use alsa::seq::{Seq, ClientIter, PortIter, PortInfo, PortCap, MidiEvent, MIDI_GENERIC, SYNTH};
    use ::errors::PortInfoError;

    pub fn poll(fds: &mut [::libc::pollfd], timeout: i32) -> i32 {
        unsafe { ::libc::poll(fds.as_mut_ptr(), fds.len() as ::libc::nfds_t, timeout) }
    }

    #[inline]
    pub fn get_port_count(s: &Seq, capability: PortCap) -> usize {
        ClientIter::new(s).flat_map(|c| PortIter::new(s, c.get_client()))
                          .filter(|p| p.get_type().intersects(MIDI_GENERIC | SYNTH))
                          .filter(|p| p.get_capability().intersects(capability))
                          .count()
    }

    #[inline]
    pub fn get_port_info(s: &Seq, capability: PortCap, port_number: usize) -> Option<PortInfo> {
        ClientIter::new(s).flat_map(|c| PortIter::new(s, c.get_client()))
                          .filter(|p| p.get_type().intersects(MIDI_GENERIC | SYNTH))
                          .filter(|p| p.get_capability().intersects(capability))
                          .nth(port_number)
    }

    #[inline]
    pub fn get_port_name(s: &Seq, capability: PortCap, port_number: usize) -> Result<String, PortInfoError> {
        use std::fmt::Write;

        let pinfo = match get_port_info(s, capability, port_number) {
            Some(p) => p,
            None => return Err(PortInfoError::PortNumberOutOfRange)
        };

        let cinfo = try!(s.get_any_client_info(pinfo.get_client()).map_err(|_| PortInfoError::CannotRetrievePortName));
        let mut output = String::new();
        write!(&mut output, "{} {}:{}", 
            try!(cinfo.get_name().map_err(|_| PortInfoError::CannotRetrievePortName)),
            pinfo.get_client(), // These lines added to make sure devices are listed
            pinfo.get_port()    // with full portnames added to ensure individual device names
        ).unwrap();
        Ok(output)
    }

    pub struct EventDecoder {
        ev: MidiEvent
    }

    impl EventDecoder {
        pub fn new(merge_commands: bool) -> EventDecoder {
            let coder = MidiEvent::new(0).unwrap();
            coder.enable_running_status(merge_commands);
            EventDecoder { ev: coder }
        }

        #[inline]
        pub fn get_wrapped(&mut self) -> &mut MidiEvent {
            &mut self.ev
        }
    }

    pub struct EventEncoder {
        ev: MidiEvent,
        buffer_size: u32
    }

    impl EventEncoder {
        #[inline]
        pub fn new(buffer_size: u32) -> EventEncoder {
            EventEncoder {
                ev: MidiEvent::new(buffer_size).unwrap(),
                buffer_size: buffer_size
            }
        }

        #[inline]
        pub fn get_buffer_size(&self) -> u32 {
            self.buffer_size
        }

        #[inline]
        pub fn resize_buffer(&mut self, bufsize: u32) -> Result<(), ()> {
            match self.ev.resize_buffer(bufsize) {
                Ok(_) => {
                    self.buffer_size = bufsize;
                    Ok(())
                },
                Err(_) => Err(())
            }
        }

        #[inline]
        pub fn get_wrapped(&mut self) -> &mut MidiEvent {
            &mut self.ev
        }
    }
}

const INITIAL_CODER_BUFFER_SIZE: usize = 32;

pub struct MidiInput {
    ignore_flags: Ignore,
    seq: Option<Seq>,
}

pub struct MidiInputConnection {
    subscription: Option<PortSubscribe>,
    thread: Option<JoinHandle<HandlerData>>,
    vport: i32, // TODO: probably port numbers are only u8, therefore could use Option<u8>
    trigger_send_fd: i32,
}

struct HandlerData {
    ignore_flags: Ignore,
    seq: Seq,
    trigger_rcv_fd: i32,
    callback: Box<FnMut(f64, &[u8])+Send>,
    queue_id: i32, // an input queue is needed to get timestamped events
}

impl MidiInput {
    pub fn new(client_name: &str) -> Result<Self, InitError> {
        let seq = match Seq::open(None, None, true) {
            Ok(s) => s,
            Err(_) => { return Err(InitError); }
        };
        
        let c_client_name = try!(CString::new(client_name).map_err(|_| InitError));
        try!(seq.set_client_name(&c_client_name).map_err(|_| InitError));
        
        Ok(MidiInput {
            ignore_flags: Ignore::None,
            seq: Some(seq),
        })
    }
    
    pub fn ignore(&mut self, flags: Ignore) {
        self.ignore_flags = flags;
    }
    
    pub fn port_count(&self) -> usize {
        helpers::get_port_count(self.seq.as_ref().unwrap(), READ | SUBS_READ)
    }
    
    pub fn port_name(&self, port_number: usize) -> Result<String, PortInfoError> {
        helpers::get_port_name(self.seq.as_ref().unwrap(), READ | SUBS_READ, port_number)
    }
    
    fn init_queue(&mut self) -> i32 {
        let seq = self.seq.as_mut().unwrap();
        let mut queue_id = 0;
        // Create the input queue
        if !cfg!(feature = "avoid_timestamping") {
            queue_id = seq.alloc_named_queue(unsafe { CStr::from_bytes_with_nul_unchecked(b"midir queue\0") }).unwrap();
            // Set arbitrary tempo (mm=100) and resolution (240)
            let qtempo = QueueTempo::empty().unwrap();
            qtempo.set_tempo(600_000);
            qtempo.set_ppq(240);
            seq.set_queue_tempo(queue_id, &qtempo).unwrap();
            let _ = seq.drain_output();
        }
        
        queue_id
    }
    
    fn init_trigger(&mut self) -> Result<[i32; 2], ()> {
        let mut trigger_fds = [-1, -1];
        
        if unsafe { ::libc::pipe(trigger_fds.as_mut_ptr()) } == -1 {
            Err(())
        } else {
            Ok(trigger_fds)
        }
    }
    
    fn create_port(&mut self, port_name: &CStr, queue_id: i32) -> Result<i32, ()> {
        let mut pinfo = PortInfo::empty().unwrap();
        // these functions are private, and the values are zeroed already by `empty()`
        //pinfo.set_client(0);
        //pinfo.set_port(0);
        pinfo.set_capability(WRITE | SUBS_WRITE);
        pinfo.set_type(MIDI_GENERIC | APPLICATION);
        pinfo.set_midi_channels(16);
        
        if !cfg!(feature = "avoid_timestamping") {
            pinfo.set_timestamping(true);
            pinfo.set_timestamp_real(true);
            pinfo.set_timestamp_queue(queue_id);
        }
        
        pinfo.set_name(port_name);
        match self.seq.as_mut().unwrap().create_port(&mut pinfo) {
            Ok(_) => Ok(pinfo.get_port()),
            Err(_) => Err(())
        }
    }
    
    fn start_input_queue(&mut self, queue_id: i32) {
        if !cfg!(feature = "avoid_timestamping") {
            let seq = self.seq.as_mut().unwrap();
            let _ = seq.control_queue(queue_id, EventType::Start, 0, None);
            let _ = seq.drain_output();
        }
    }
    
    pub fn connect<F>(
        mut self, port_number: usize, port_name: &str, callback: F
    ) -> Result<MidiInputConnection, ConnectError<Self>>
        where F: FnMut(f64, &[u8]) + Send + 'static {
        
        let trigger_fds = match self.init_trigger() {
            Ok(fds) => fds,
            Err(()) => { return Err(ConnectError::other("could not create communication pipe for ALSA handler", self)); }
        };
        
        let queue_id = self.init_queue();

        let src_pinfo = match helpers::get_port_info(self.seq.as_ref().unwrap(), READ | SUBS_READ, port_number) {
            Some(p) => p,
            None => return Err(ConnectError::new(ConnectErrorKind::PortNumberOutOfRange, self))
        };

        let c_port_name = match CString::new(port_name) {
            Ok(c_port_name) => c_port_name,
            Err(_) => return Err(ConnectError::other("port_name must not contain null bytes", self))
        };
        
        let vport = match self.create_port(&c_port_name, queue_id) {
            Ok(vp) => vp,
            Err(_) => {
                return Err(ConnectError::other("could not create ALSA input port", self));
            }
        };
        
        // Make subscription
        let sub = PortSubscribe::empty().unwrap();
        sub.set_sender(Addr { client: src_pinfo.get_client(), port: src_pinfo.get_port()});
        sub.set_dest(Addr { client: self.seq.as_ref().unwrap().client_id().unwrap(), port: vport});
        if self.seq.as_ref().unwrap().subscribe_port(&sub).is_err() {
            return Err(ConnectError::other("could not create ALSA input subscription", self));
        }
        let subscription = sub;
        
        // Start the input queue
        self.start_input_queue(queue_id);

        // Start our MIDI input thread.
        let handler_data = HandlerData {
            ignore_flags: self.ignore_flags,
            seq: self.seq.take().unwrap(),
            trigger_rcv_fd: trigger_fds[0],
            callback: Box::new(callback),
            queue_id: queue_id
        };
        
        let threadbuilder = Builder::new();
        let name = format!("midir ALSA input handler (port '{}')", port_name);
        let threadbuilder = threadbuilder.name(name);
        let thread = match threadbuilder.spawn(move || {
            let mut d = data;
            let h = handle_input(handler_data, &mut d);
            (h, d) // return both the handler data and the user data 
        }) {
            Ok(handle) => handle,
            Err(_) => {
                //unsafe { snd_seq_unsubscribe_port(self.seq.as_mut_ptr(), sub.as_ptr()) };
                return Err(ConnectError::other("could not start ALSA input handler thread", self));
            }
        };

        Ok(MidiInputConnection {
            subscription: Some(subscription),
            thread: Some(thread),
            vport: vport,
            trigger_send_fd: trigger_fds[1]
        })
    }
    
    pub fn create_virtual<F>(
        mut self, port_name: &str, callback: F
    ) -> Result<MidiInputConnection, ConnectError<Self>>
    where F: FnMut(f64, &[u8]) + Send + 'static {
        let trigger_fds = match self.init_trigger() {
            Ok(fds) => fds,
            Err(()) => { return Err(ConnectError::other("could not create communication pipe for ALSA handler", self)); }
        };
        
        let queue_id = self.init_queue();

        let c_port_name = match CString::new(port_name) {
            Ok(c_port_name) => c_port_name,
            Err(_) => return Err(ConnectError::other("port_name must not contain null bytes", self))
        };
        
        let vport = match self.create_port(&c_port_name, queue_id) {
            Ok(vp) => vp,
            Err(_) => {
                return Err(ConnectError::other("could not create ALSA input port", self));
            }
        };
        
        // Start the input queue
        self.start_input_queue(queue_id);
        
        // Start our MIDI input thread.
        let handler_data = HandlerData {
            ignore_flags: self.ignore_flags,
            seq: self.seq.take().unwrap(),
            trigger_rcv_fd: trigger_fds[0],
            callback: Box::new(callback),
            queue_id: queue_id
        };
        
        let threadbuilder = Builder::new();
        let thread = match threadbuilder.spawn(move || {
            let mut d = data;
            let h = handle_input(handler_data, &mut d);
            (h, d) // return both the handler data and the user data 
        }) {
            Ok(handle) => handle,
            Err(_) => {
                //unsafe { snd_seq_unsubscribe_port(self.seq.as_mut_ptr(), sub.as_ptr()) };
                return Err(ConnectError::other("could not start ALSA input handler thread", self));
            }
        };

        Ok(MidiInputConnection {
            subscription: None,
            thread: Some(thread),
            vport: vport,
            trigger_send_fd: trigger_fds[1]
        })
    }
}

impl MidiInputConnection {
    pub fn close(mut self) -> MidiInput {
        let handler_data = self.close_internal();
        
        MidiInput {
            ignore_flags: handler_data.ignore_flags,
            seq: Some(handler_data.seq),
        }
    }
    
    /// This must only be called if the handler thread has not yet been shut down
    fn close_internal(&mut self) -> HandlerData {
        // Request the thread to stop.
        let _res = unsafe { ::libc::write(self.trigger_send_fd, &false as *const bool as *const _, mem::size_of::<bool>() as ::libc::size_t) };
        
        let thread = self.thread.take().unwrap(); 
        // Join the thread to get the handler_data back
        let handler_data = thread.join().unwrap(); // TODO: don't use unwrap here
        
        // TODO: find out why snd_seq_unsubscribe_port takes a long time if there was not yet any input message
        if let Some(ref subscription) = self.subscription {
            let _ = handler_data.seq.unsubscribe_port(subscription.get_sender(), subscription.get_dest());
        }
        
        // Close the trigger fds (TODO: make sure that these are closed even in the presence of panic in thread)
        unsafe {
            ::libc::close(handler_data.trigger_rcv_fd);
            ::libc::close(self.trigger_send_fd);
        }
        
        // Stop and free the input queue
        if !cfg!(feature = "avoid_timestamping") {
            let _ = handler_data.seq.control_queue(handler_data.queue_id, EventType::Stop, 0, None);
            let _ = handler_data.seq.drain_output();
            let _ = handler_data.seq.free_queue(handler_data.queue_id);
        }
        
        // Delete the port
        let _ = handler_data.seq.delete_port(self.vport);
        
        handler_data
    }
}


impl Drop for MidiInputConnection {
    fn drop(&mut self) {
        // Use `self.thread` as a flag whether the connection has already been dropped
        if self.thread.is_some() {
            self.close_internal();
        }
    }
}

pub struct MidiOutput {
    seq: Option<Seq>, // TODO: if `Seq` is marked as non-zero, this should just be pointer-sized 
}

pub struct MidiOutputConnection {
    seq: Option<Seq>,
    vport: i32,
    coder: helpers::EventEncoder,
    subscription: Option<PortSubscribe>
}

impl MidiOutput {
    pub fn new(client_name: &str) -> Result<Self, InitError> {
        let seq = match Seq::open(None, Some(Direction::Playback), true) {
            Ok(s) => s,
            Err(_) => { return Err(InitError); }
        };
        
        let c_client_name = try!(CString::new(client_name).map_err(|_| InitError));
        try!(seq.set_client_name(&c_client_name).map_err(|_| InitError));
        
        Ok(MidiOutput {
            seq: Some(seq),
        })
    }
    
    pub fn port_count(&self) -> usize {
        helpers::get_port_count(self.seq.as_ref().unwrap(), WRITE | SUBS_WRITE)
    }
    
    pub fn port_name(&self, port_number: usize) -> Result<String, PortInfoError> {
        helpers::get_port_name(self.seq.as_ref().unwrap(), WRITE | SUBS_WRITE, port_number)
    }
    
    pub fn connect(mut self, port_number: usize, port_name: &str) -> Result<MidiOutputConnection, ConnectError<Self>> {
        let pinfo = match helpers::get_port_info(self.seq.as_ref().unwrap(), WRITE | SUBS_WRITE, port_number) {
            Some(p) => p,
            None => return Err(ConnectError::new(ConnectErrorKind::PortNumberOutOfRange, self))
        };

        let c_port_name = match CString::new(port_name) {
            Ok(c_port_name) => c_port_name,
            Err(_) => return Err(ConnectError::other("port_name must not contain null bytes", self))
        };

        let vport = match self.seq.as_ref().unwrap().create_simple_port(&c_port_name, READ | SUBS_READ, MIDI_GENERIC | APPLICATION) {
            Ok(vport) => vport,
            Err(_) => return Err(ConnectError::other("could not create ALSA output port", self))
        };

        // Make subscription
        let sub = PortSubscribe::empty().unwrap();
        sub.set_sender(Addr { client: self.seq.as_ref().unwrap().client_id().unwrap(), port: vport });
        sub.set_dest(Addr { client: pinfo.get_client(), port: pinfo.get_port() });
        sub.set_time_update(true);
        sub.set_time_real(true);
        if self.seq.as_ref().unwrap().subscribe_port(&sub).is_err() {
            return Err(ConnectError::other("could not create ALSA output subscription", self));
        }
        
        Ok(MidiOutputConnection {
            seq: self.seq.take(),
            vport: vport,
            coder: helpers::EventEncoder::new(INITIAL_CODER_BUFFER_SIZE as u32),
            subscription: Some(sub)
        })
    }
    
    pub fn create_virtual(
        mut self, port_name: &str
    ) -> Result<MidiOutputConnection, ConnectError<Self>> {
        let c_port_name = match CString::new(port_name) {
            Ok(c_port_name) => c_port_name,
            Err(_) => return Err(ConnectError::other("port_name must not contain null bytes", self))
        };

        let vport = match self.seq.as_ref().unwrap().create_simple_port(&c_port_name, READ | SUBS_READ, MIDI_GENERIC | APPLICATION) {
            Ok(vport) => vport,
            Err(_) => return Err(ConnectError::other("could not create ALSA output port", self))
        };
        
        Ok(MidiOutputConnection {
            seq: self.seq.take(),
            vport: vport,
            coder: helpers::EventEncoder::new(INITIAL_CODER_BUFFER_SIZE as u32),
            subscription: None
        })
    }
}
        

impl MidiOutputConnection {
    pub fn close(mut self) -> MidiOutput {
        self.close_internal();
        
        MidiOutput {
            seq: self.seq.take(),
        }
    }
    
    pub fn send(&mut self, message: &[u8]) -> Result<(), SendError> {  
        let nbytes = message.len();
        assert!(nbytes <= u32::max_value() as usize);
        
        if nbytes > self.coder.get_buffer_size() as usize {
            if self.coder.resize_buffer(nbytes as u32).is_err() {
                return Err(SendError::Other("could not resize ALSA encoding buffer"));
            }
        }
        
        let mut ev = match self.coder.get_wrapped().encode(message) {
            Ok((_, Some(ev))) => ev,
            _ => return Err(SendError::InvalidData("ALSA encoder reported invalid data"))
        };

        ev.set_source(self.vport);
        ev.set_subs();
        ev.set_direct();
        
        // Send the event.
        if self.seq.as_ref().unwrap().event_output(&mut ev).is_err() {
            return Err(SendError::Other("could not send encoded ALSA message"));
        }
        
        let _ = self.seq.as_mut().unwrap().drain_output();
        Ok(())
    }
    
    fn close_internal(&mut self) {
        let seq = self.seq.as_mut().unwrap();
        if let Some(ref subscription) = self.subscription {
            let _ = seq.unsubscribe_port(subscription.get_sender(), subscription.get_dest());
        }
        let _ = seq.delete_port(self.vport);
    }
}

impl Drop for MidiOutputConnection {
    fn drop(&mut self) {
        if self.seq.is_some() {
            self.close_internal();
        }
    }
}

fn handle_input(mut data: HandlerData) -> HandlerData {
    use alsa::PollDescriptors;
    use alsa::seq::{EventType, Connect};

    let mut last_time: Option<u64> = None;
    let mut continue_sysex: bool = false;
    
    // ALSA documentation says:
    // The required buffer size for a sequencer event it as most 12 bytes, except for System Exclusive events (which we handle separately)
    let mut buffer = [0; 12];
    
    let mut coder = helpers::EventDecoder::new(false);
    
    let mut poll_fds: Box<[::libc::pollfd]>;
    {
        let poll_desc_info = (&data.seq, Some(Direction::Capture));
        let poll_fd_count = poll_desc_info.count() + 1;
        let mut vec = Vec::with_capacity(poll_fd_count);
        unsafe {    
            vec.set_len(poll_fd_count);
            poll_fds = vec.into_boxed_slice();
        }
        poll_desc_info.fill(&mut poll_fds[1..]).unwrap();
    }
    poll_fds[0].fd = data.trigger_rcv_fd;
    poll_fds[0].events = ::libc::POLLIN;

            
    let mut message = MidiMessage::new();

    { // open scope where we can borrow data.seq
    let mut seq_input = data.seq.input();
    
    let mut do_input = true;
    while do_input {
        if let Ok(0) = seq_input.event_input_pending(true) {
            // No data pending
            if helpers::poll(&mut poll_fds, -1) >= 0 {
                // Read from our "channel" whether we should stop the thread 
                if poll_fds[0].revents & ::libc::POLLIN != 0 {
                    let _res = unsafe { ::libc::read(poll_fds[0].fd, mem::transmute(&mut do_input), mem::size_of::<bool>() as ::libc::size_t) };
                }
            }
            continue;
        }

        // This is a bit weird, but we now have to decode an ALSA MIDI
        // event (back) into MIDI bytes. We'll ignore non-MIDI types.

        // The ALSA sequencer has a maximum buffer size for MIDI sysex
        // events of 256 bytes. If a device sends sysex messages larger
        // than this, they are segmented into 256 byte chunks.    So,
        // we'll watch for this and concatenate sysex chunks into a
        // single sysex message if necessary.
        //
        // TODO: Figure out if this is still true (seems to not be the case)
        //       If not (i.e., each event represents a complete message), we can
        //       call the user callback with the byte buffer directly, without the
        //       copying to `message.bytes` first.
        if !continue_sysex { message.bytes.clear() }

        let ignore_flags = data.ignore_flags;

        // If here, there should be data.
        let mut ev = match seq_input.event_input() {
            Ok(ev) => ev,
            Err(ref e) if e.code() == -::libc::ENOSPC => {
                let _ = writeln!(stderr(), "\nError in handle_input: ALSA MIDI input buffer overrun!\n");
                continue;
            },
            Err(ref e) if e.code() == -::libc::EAGAIN => {
                let _ = writeln!(stderr(), "\nError in handle_input: no input event from ALSA MIDI input buffer!\n");
                continue;
            },
            Err(ref e) => {
                let _ = writeln!(stderr(), "\nError in handle_input: unknown ALSA MIDI input error ({})!\n", e.code());
                //perror("System reports");
                continue;
            }
        };
        
        let do_decode = match ev.get_type() {
            EventType::PortSubscribed => {
                if cfg!(debug) { println!("Notice from handle_input: ALSA port connection made!") };
                false
            },
            EventType::PortUnsubscribed => {
                if cfg!(debug) {
                    let _ = writeln!(stderr(), "Notice from handle_input: ALSA port connection has closed!");
                    let connect = ev.get_data::<Connect>().unwrap();
                    let _ = writeln!(stderr(), "sender = {}:{}, dest = {}:{}",
                        connect.sender.client,
                        connect.sender.port,
                        connect.dest.client,
                        connect.dest.port
                    );
                }
                false
            },
            EventType::Qframe => { // MIDI time code
                !ignore_flags.contains(Ignore::Time)
            },
            EventType::Tick => { // 0xF9 ... MIDI timing tick
                !ignore_flags.contains(Ignore::Time)
            },
            EventType::Clock => { // 0xF8 ... MIDI timing (clock) tick
                !ignore_flags.contains(Ignore::Time)
            },
            EventType::Sensing => { // Active sensing
                !ignore_flags.contains(Ignore::ActiveSense)
            },
            EventType::Sysex => {
                if !ignore_flags.contains(Ignore::Sysex) {
                    // Directly copy the data from the external buffer to our message
                    message.bytes.extend_from_slice(ev.get_ext().unwrap());
                    continue_sysex = *message.bytes.last().unwrap() != 0xF7;
                }
                false // don't ever decode sysex messages (it would unnecessarily copy the message content to another buffer)
            },
            _ => true
        };

        // NOTE: SysEx messages have already been "decoded" at this point!
        if do_decode {
            if let Ok(nbytes) = coder.get_wrapped().decode(&mut buffer, &mut ev) {
                if nbytes > 0 {
                    message.bytes.extend_from_slice(&buffer[0..nbytes]);
                }
            }
        }

        if message.bytes.len() == 0 || continue_sysex { continue; }

        // Calculate the time stamp:
        // Use the ALSA sequencer event time data.
        // (thanks to Pedro Lopez-Cabanillas!).
        let alsa_time = ev.get_time().unwrap();
        let secs = alsa_time.as_secs();
        let nsecs = alsa_time.subsec_nanos();

        let timestamp = ( secs as u64 * 1_000_000 ) + ( nsecs as u64/1_000 );
        message.timestamp = match last_time {
            None => 0.0,
            Some(last) => (timestamp - last) as f64 * 0.000001
        };
        last_time = Some(timestamp);
        
        (data.callback)(message.timestamp, &message.bytes);
    }
    
    } // close scope where data.seq is borrowed
    data // return data back to thread owner
}
