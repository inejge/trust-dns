// Copyright (C) 2015 Benjamin Fry <benjaminfry@me.com>
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//      http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.
use std::net::SocketAddr;
use std::io::{Write, Read};
use std::mem;
use std::fmt;

use mio::tcp::TcpStream;
use mio::{Token, EventLoop, Handler, EventSet, PollOpt}; // not * b/c don't want confusion with std::net

use ::error::*;
use ::serialize::binary::*;
use client::ClientConnection;

const RESPONSE: Token = Token(0);

pub struct TcpClientConnection {
  socket: Option<TcpStream>,
  event_loop: EventLoop<Response>,
}

impl TcpClientConnection {
  pub fn new(name_server: SocketAddr) -> ClientResult<Self> {
    debug!("connecting to {:?}", name_server);
    let stream = try!(TcpStream::connect(&name_server));

    let mut event_loop: EventLoop<Response> = try!(EventLoop::new());
    // TODO make the timeout configurable, 5 seconds is the dig default
    // TODO the error is private to mio, which makes this awkward...
    if event_loop.timeout_ms((), 5000).is_err() { return Err(ClientError::TimerError) };

    Ok(TcpClientConnection{ socket: Some(stream), event_loop: event_loop })
  }
}

impl ClientConnection for TcpClientConnection {
  fn send(&mut self, buffer: Vec<u8> ) -> ClientResult<Vec<u8>> {

    try!(self.event_loop.reregister(self.socket.as_ref().expect("never none"), RESPONSE, EventSet::all(), PollOpt::all()));
    let mut response: Response = Response::new(buffer, mem::replace(&mut self.socket, None).expect("Only one user at a time"));
    try!(self.event_loop.run(&mut response));


    if response.error.is_some() { return Err(response.error.unwrap()) }
    if response.buf.is_none() { return Err(ClientError::NoDataReceived) }
    let result = Ok(response.buf.unwrap());
    self.socket = Some(response.stream);
    result
  }
}

impl fmt::Debug for TcpClientConnection {
  fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
    write!(f, "TcpClientConnection: {:?}", self.socket)
  }
}

struct Response {
  pub state: ClientState,
  pub message: Vec<u8>,
  pub buf: Option<Vec<u8>>,
  pub error: Option<ClientError>,
  pub stream: TcpStream,
}

enum ClientState {
  WillWrite,
  //WillRead,
}

impl Response {
  pub fn new(message: Vec<u8>, stream: TcpStream) -> Self {
    Response{ state: ClientState::WillWrite, message: message, buf: None, error: None, stream: stream }
  }
}

// TODO: this should be merged with the server handler
impl Handler for Response {
  type Timeout = ();
  type Message = ();

  fn ready(&mut self, event_loop: &mut EventLoop<Self>, token: Token, events: EventSet) {
    match token {
      RESPONSE => {
        if events.is_writable() {
          let len: [u8; 2] = [(self.message.len() >> 8 & 0xFF) as u8, (self.message.len() & 0xFF) as u8];
          self.error = self.stream.write_all(&len).and_then(|_|self.stream.write_all(&self.message)).err().map(|o|o.into());
          if self.error.is_some() { return }

          self.error = self.stream.flush().err().map(|o|o.into());
          debug!("wrote {} bytes to {:?}", self.message.len(), self.stream.peer_addr());
        } else if events.is_readable() {
          // assuming we will always be able to read two bytes.
          let mut len_bytes: [u8;2] = [0u8;2];

          {
            let stream: &mut TcpStream = &mut self.stream;
            match stream.take(2).read(&mut len_bytes) {
              Ok(len) if len != 2 => {
                debug!("did not read all len_bytes expected: 2 got: {:?} bytes from: {:?}", len_bytes, stream);
                self.error = Some(ClientError::NotAllBytesReceived{received: len, expect: 2});
                return
              },
              Err(e) => {
                self.error = Some(e.into());
                return
              },
              Ok(_) => (),
            }
          }

          let len: u16 = (len_bytes[0] as u16) << 8 & 0xFF00 | len_bytes[1] as u16 & 0x00FF;

          debug!("reading {:?} bytes from: {:?}", len, self.stream.peer_addr());
          // use a cursor here, and seek to the write spot on each read...
          let mut buf = Vec::with_capacity(len as usize);
          {
            let stream: &mut TcpStream = &mut self.stream;
            match stream.take(len as u64).read_to_end(&mut buf) {
              Ok(got) if got != len as usize => {
                debug!("did not read all bytes got: {} expected: {} bytes from: {:?}", got, len, stream.peer_addr());
                self.error = Some(ClientError::NotAllBytesReceived{received: got, expect: len as usize});
                return
              },
              Err(e) => {
                self.error = Some(e.into());
                return
              },
              Ok(_) => (),
            }
          }

          // we got our response, shutdown.
          event_loop.shutdown();

          debug!("read {:?} bytes from: {:?}", buf.len(), self.stream);

          // set our data
          self.buf = Some(buf);

          // TODO, perhaps parse the response in here, so that the client could ignore messages with the
          //  wrong serial number.
        } else if events.is_error() || events.is_hup() {
          debug!("an error occured, connection shutdown early: {:?}", token);
          self.error = Some(ClientError::NoDataReceived);
          event_loop.shutdown();
        } else {
          debug!("got woken up, but not readable or writable: {:?}", token);
          return
        }
      },
      _ => {
        error!("unrecognized token: {:?}", token);
        self.error = Some(ClientError::NoDataReceived);
      },
    }
  }

  fn timeout(&mut self, event_loop: &mut EventLoop<Self>, _: ()) {
    self.error = Some(ClientError::TimedOut);
    event_loop.shutdown();
  }
}

// TODO: should test this independently of the client code