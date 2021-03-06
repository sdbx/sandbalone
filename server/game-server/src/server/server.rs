use tool::print_err;
use model::EndedCause;
use std::io::BufReader;
use std::io::prelude::*;
use model::UserId;
use model::User;
use rand::Rng;
use server::task;
use model::Event;
use std::net::Shutdown;
use std::time::Duration;
use model::{Task, TaskResult, CompleteGameResult, TaskRequest, GameServer};
use redis::Commands;
use error::Error;
use std::error::{Error as SError};
use ticker::Ticker;
use game::{Game, Player, Board, Cord, Stone};
use std::sync::mpsc::Sender;
use std::sync::mpsc::channel;
use std::sync::mpsc::Receiver;
use redis::{Client, PubSub};
use uuid::Uuid;
use std::net::{TcpStream, TcpListener};
use model::{parse_command, Command, Invite};
use server::room::Room;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::io::{Read, Write};
use std::thread;
use server::cmd;

pub enum ServerEvent {
    Connect{ conn_id: Uuid, conn: TcpStream },
    Dispatch { conn_id: Uuid, event: Event },
    Close { conn_id: Uuid },
    DiscoverUpdated,
    TimeUpdated { dt: usize },
    Command { conn_id: Uuid, cmd: Command },
    TaskRequest { task_request: TaskRequest }
}

#[derive(Clone, Debug)]
pub struct Connection {
    pub conn_id: Uuid,
    pub user_id: UserId,
    pub room_id: Option<String>
}

pub struct Server {
    tx: Option<Sender<ServerEvent>>,
    addr: String,
    pub real_addr: String,
    pub name: String,
    pub redis: Client,
    pub rooms: HashMap<String, Room>,
    pub invites: HashMap<String, Invite>,
    pub conns: HashMap<Uuid, Connection>,
    pub streams: HashMap<Uuid, TcpStream>
}

const redis_server_hash: &'static str = "game_server_hash";
const redis_lobby_queue: &'static str = "task_lobby_queue";
const result_timeout: usize = 5;

fn redis_game_queue(server: &str) -> String {
    format!("task_game_queue_{}", server)
} 

fn redis_result_channel(id: &str) -> String {
    format!("task_result_chan_{}", id)
} 

impl Server {
    pub fn new(addr: &str, name: &str, real_addr: &str, redis_addr: &str) -> Self {
        Self {
            tx: None,
            name: name.to_owned(),
            addr: addr.to_owned(),
            real_addr: real_addr.to_owned(),
            redis: Client::open(redis_addr).unwrap(),
            rooms: HashMap::new(),
            invites: HashMap::new(),
            conns: HashMap::new(),
            streams: HashMap::new()
        }
    }

    pub fn get_room(&self, conn: &Connection) -> Result<&Room, Error> {
        let room_id = match conn.room_id {
            Some(ref x) => x,
            None => return Err(Error::Permission)
        };
        Ok(self.rooms.get(room_id)?)
    }

    pub fn get_room_mut(&mut self, conn: &Connection) -> Result<&mut Room, Error> {
        let room_id = match conn.room_id {
            Some(ref x) => x,
            None => return Err(Error::Permission)
        };
        Ok(self.rooms.get_mut(room_id)?)
    }

    pub fn delete_room(&mut self, room_id: &str) {
        self.rooms.remove(&room_id.to_owned());
        let keys = self.invites.iter()
            .filter(|(_,x)| x.room_id == room_id)
            .map(|(k,_)| k.clone())
            .collect::<Vec<_>>();
        keys.iter().for_each(|x| {
            self.rooms.remove(x);
        });
    }

    pub fn get_invite_of_user(&self, user_id: UserId, room_id: &str) -> Option<Invite> {
        match self.invites.iter().find(|(_, x)| x.user_id == user_id && x.room_id == room_id) {
            Some((_, x)) => Some(x.clone()),
            None => None
        }
    }

    pub fn serve(mut self) {
        for event in self.listen().iter() {
            print_err(self.handle_event(event));
        }
    }

    pub fn update_discover(&self) -> Result<(), Error> {
        let conn = self.redis.get_connection()?;
        let game_server = GameServer::from_server(self);
        let buf = serde_json::to_string(&game_server)?;
        let _: () = conn.hset(redis_server_hash, &self.name, &buf)?;
        Ok(())
    }

    pub fn dispatch(&mut self, conn_id: Uuid, event: &Event) {
        if let Some(stream) = self.streams.get_mut(&conn_id) {
            let msg = serde_json::to_string(&event).unwrap() + "\n";
            print_err(stream.write(msg.as_bytes()));
            print_err(stream.flush());
        }
    }

    pub fn destroy_room(&mut self, room_id: &str) {
        let users = {
            if let Some(room) = self.rooms.get(room_id) {
                room.users.clone()
            } else {
                HashMap::new()
            }
        };
        for (_, user) in users.iter() {
            self.kick(user.conn_id);
        }
    }

    pub fn broadcast(&mut self, room_id: &str, event: &Event) {
        let conn_ids = {
            let room = self.rooms.get(room_id).unwrap();
            room.users.iter().map(|t| t.1.conn_id.clone()).collect::<Vec<_>>()
        };
        conn_ids.iter().for_each(|conn_id| self.dispatch(*conn_id, &event));
    }

    pub fn kick(&mut self, conn_id: Uuid) {
        if let Some(stream) = self.streams.get(&conn_id) {
            print_err(stream.shutdown(Shutdown::Both));
        }
    }

    pub fn complete_game(&mut self, room_id: &str, loser: Player, cause: &EndedCause) -> Result<(), Error> {
        let (game, is_rank) = {
            let room = match self.rooms.get_mut(room_id) {
                Some(x) => x,
                None => return Err(Error::Internal)
            };
            if room.rank.is_some() {
                println!("[room:{}] ranked game ended", room_id);
            } else {
                println!("[room:{}] game ended", room_id);
            }
            
            let game = match room.game.as_ref() {
                Some(x) => x.clone(),
                None => return Err(Error::Internal)
            };
            room.game = None;
            if room.rank.is_some() {
                let rank = room.rank.as_mut().unwrap();
                rank.black = UserId::empty;
                rank.white = UserId::empty;
                rank.time = 10*1000;
            }
            (game, room.rank.is_some())
        };

        let event = {
            let user = if loser == Player::Black {
                game.black
            } else {
                game.white
            };
            let task = Task::CompleteGame {
                black: game.black,
                white: game.white,
                rank: is_rank,
                loser: loser.to_string(),
                cause: cause.clone(),
                map: game.map,
                game_rule: game.rule,
                moves: game.history
            };
            
            let buf = match self.request_task(&task) {
                Ok(x) => x,
                Err(_) => return Err(Error::Internal)
            };
            
            let res2: CompleteGameResult = serde_json::from_str(&buf)?;
            
            Event::Ended {
                loser: user,
                winner_delta: res2.winner_delta,
                loser_delta: res2.loser_delta,
                player: loser.to_string(),
                cause: cause.clone()
            }
        };
        self.broadcast(&room_id, &event);
        Ok(())
    }

    pub fn request_task(&mut self, task: &Task) -> Result<String, Error> {
        let conn = self.redis.get_connection()?;
        let id = Uuid::new_v4().to_string();
        let buf = serde_json::to_string(&TaskRequest {
            id: id.clone(),
            task: task.clone()
        })?;
        conn.rpush(redis_lobby_queue, &buf)?;
        let buf: Vec<String> = conn.blpop(redis_result_channel(&id), result_timeout)?;
        let res: TaskResult = serde_json::from_str(buf.get(1)?)?;
        if let Some(error) = res.error {
            Err(Error::TaskError(error))
        } else {
            Ok(res.value)
        }
    }

    fn handle_event(&mut self, event: ServerEvent) -> Result<(), Error> {
        match event {
            ServerEvent::Connect { conn_id, conn } => {
                println!("[socket] client({}) connected", conn_id);
                self.conns.insert(conn_id, Connection {
                    conn_id: conn_id,
                    user_id: UserId::empty,
                    room_id: None
                });
                self.streams.insert(conn_id, conn);
            },
            ServerEvent::Command { conn_id, cmd } => {
                let conn = { 
                    self.conns.get(&conn_id).unwrap().clone()
                };
                if let Err(err) = cmd::handle(self, &conn, &cmd) {
                    self.dispatch(conn_id, &Event::Error{message: format!("{}", err)});
                    match err {
                        Error::ShouldTerminate(_) => {
                            self.kick(conn_id);
                        },
                        _ => {}
                    }
                };
            },
            ServerEvent::DiscoverUpdated => {
                self.update_discover()?;
            },
            ServerEvent::TimeUpdated { dt }=> {
                let mut events: Vec<(String, Event)> = Vec::new();
                let mut completes: Vec<(String, Player, EndedCause)> = Vec::new();
                let mut closes: Vec<String> = Vec::new();
                for (_, room) in self.rooms.iter_mut() {
                    if room.rank.is_some() && room.game.is_none() {
                        // TODO use just isize
                        let rank = {
                            let rank = room.rank.as_mut().unwrap();
                            if rank.time <= 0 { 
                                closes.push(room.id.clone());
                            } else {
                                rank.time -= (dt as isize);
                            }
                            rank.clone()
                        };

                        if room.exists_user(rank.black) && room.exists_user(rank.white) {
                            print_err(room.start());
                            if let Some(ref game) = room.game {
                                events.push((room.id.clone(), Event::game_to_started(game)));
                            }
                        }
                    }
                    if let Some(game) = room.game.as_mut() {
                        game.time_update(dt);
                        if let Some((loser, cause)) = game.get_lose() {
                            completes.push((room.id.clone(), loser, cause));
                        }
                    }
                }
                for (room_id, event) in events.iter() {
                    self.broadcast(&room_id, &event);
                }
                for (room_id, loser, cause) in completes.iter() {
                    print_err(self.complete_game(&room_id, *loser, &cause));
                }
                for room_id in closes.iter() {
                    self.destroy_room(room_id);
                }
            },
            ServerEvent::TaskRequest { task_request } => {
                match task::handle(self, task_request.task) {
                    Ok(res) => {
                        println!("[task] task({}) was successful: {}", task_request.id, res);
                        print_err(self.send_result(&task_request.id, &TaskResult{
                            error: None,
                            value: res
                        }));
                    },
                    Err(e) => {
                        println!("[task] task({}) encounterd an error: {}", task_request.id, e);
                        print_err(self.send_result(&task_request.id, &TaskResult{
                            error: Some(format!("{}", e)),
                            value: "".to_owned()
                        }));
                    }
                };
            },
            ServerEvent::Close { conn_id } => {
                println!("[socket] client({}) disconnected", conn_id);
                let (room_id, user_id, user_len, conf, is_rank) = {
                    let conn = self.conns.get(&conn_id)?;
                    let room_id = conn.room_id.as_ref()?;
                    let user_id = conn.user_id;
                    let mut room = self.rooms.get_mut(room_id)?;

                    room.users.remove(&conn_id);

                    let user_len = room.users.len();
                    let old = room.conf.clone();
                    //designate a new king randomly
                    if user_id == room.conf.king && user_len != 0 {
                        let values = room.users.values().collect::<Vec<_>>();
                        room.conf.king = rand::thread_rng().choose(&values).unwrap().user_id;
                    }
                    //empty black or white if not ingame
                    if room.game.is_none() {
                        if user_id == room.conf.black {
                            room.conf.black = UserId::empty;
                        }
                        if user_id == room.conf.white {
                            room.conf.white = UserId::empty;
                        }
                    }

                    (room_id.clone(), user_id, user_len, if old != room.conf { Some(room.conf.clone()) } else { None }, room.rank.is_some())
                };

                if user_len == 0 && !is_rank {
                    //delete empty room
                    self.delete_room(&room_id)
                } else {
                    self.broadcast(&room_id, &Event::Left {
                        user: user_id
                    });
                    if let Some(conf) = conf {
                        self.broadcast(&room_id, &Event::Confed{
                            conf: conf
                        });
                    }
                }
                self.streams.remove(&conn_id);
                self.conns.remove(&conn_id);
                self.update_discover()?;
            },
            ServerEvent::Dispatch { conn_id, event } => {
                self.dispatch(conn_id, &event);
            }
        }
        Ok(())
    }
    
    fn make_tx(&self) -> Sender<ServerEvent> {
        let tx = self.tx.as_ref().unwrap();
        tx.clone()
    }

    fn send_result(&self, id: &str, res: &TaskResult) -> Result<(), Error> {
        let conn = self.redis.get_connection()?;
        let buf = serde_json::to_string(&res)?;
        Ok(conn.rpush(redis_result_channel(id), buf)?)
    }
    
    fn listen(&mut self) -> Receiver<ServerEvent> {
        let (tx, rx) = channel();
        self.tx = Some(tx.clone());
        self.listen_socket();
        self.listen_task().unwrap();
        self.ping_update();
        self.time_update(30);
        rx
    }

    fn ping_update(&self) {
        let tx = self.make_tx();
        thread::spawn(move || {
            print_err(tx.send(ServerEvent::DiscoverUpdated));
            for _ in Ticker::new(0.., Duration::from_secs(5)) {
                print_err(tx.send(ServerEvent::DiscoverUpdated));
            }
        });
    }

    fn time_update(&self, dt: usize) {
        let tx = self.make_tx();
        thread::spawn(move || {
            print_err(tx.send(ServerEvent::TimeUpdated{dt}));
            for _ in Ticker::new(0.., Duration::from_millis(dt as u64)) {
                print_err(tx.send(ServerEvent::TimeUpdated{dt}));
            }
        });
    }

    fn listen_socket(&mut self) {
        let tx = self.make_tx();
        let listener = TcpListener::bind(&self.addr).unwrap();
        thread::spawn(move || {
            for t in listener.incoming() {
                if let Ok(mut stream) = t {
                    Server::handle_stream(stream, tx.clone());
                }
            }
        });
   }

    fn handle_stream(mut stream: TcpStream, tx: Sender<ServerEvent>) {
        let conn_id = Uuid::new_v4();
        print_err(tx.send(ServerEvent::Connect{
            conn_id: conn_id,
            conn: stream.try_clone().unwrap()
        }));
        
        thread::spawn(move || {
            Server::handle_stream_loop(conn_id, &tx, stream);
            print_err(tx.send(ServerEvent::Close{
                conn_id
            }));
        });
    }

    fn handle_stream_loop(conn_id: Uuid, tx: &Sender<ServerEvent>, stream: TcpStream) {
        let mut reader = BufReader::new(stream);
        let mut msg = String::new();
        loop {
            match reader.read_line(&mut msg) {
                Ok(size) => {
                    if size == 0 {
                        break;
                    }
                    println!("[socket] client({}) sent msg: {}", conn_id, msg);
                    let t = parse_command(&msg.trim_matches('\0'));
                    msg.clear(); 
                    if let Ok(cmd) = t {
                        println!("[socket] client({}) command: {:?}", conn_id, cmd);
                        print_err(tx.send(ServerEvent::Command{
                            conn_id,
                            cmd
                        }));
                    } else {
                        print_err(tx.send(ServerEvent::Dispatch {
                            conn_id,
                            event: Event::Error {
                                message: "json format error".to_owned()
                            }
                        }));
                    }
                },
                Err(e) => {
                    println!("[socket] error: {}", e);
                    break;
                }
            }
        }
    }

   fn listen_task(&mut self) -> Result<(), Error> {
        let conn = self.redis.get_connection()?;
        let name = self.name.clone();
        let tx = self.make_tx();
        thread::spawn(move || {
            loop {
                let res: Vec<String> = match conn.blpop(redis_game_queue(&name), 0) {
                    Ok(res) => res,
                    Err(e) => { println!("{}", e); continue }
                };
                let task_request: TaskRequest = match serde_json::from_str(&res.get(1).unwrap()) {
                    Ok(task) => task,
                    Err(e) => { println!("{}", e); continue }
                };
                println!("[task] task ({}) arrived: {:?}", task_request.id, task_request.task);
                print_err(tx.send(ServerEvent::TaskRequest { task_request }));
            }
        });
        Ok(())
   }
}