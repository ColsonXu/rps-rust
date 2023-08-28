use std::borrow::Cow;
use std::collections::HashMap;
use std::fmt::{self, Display, Formatter};
use std::ops::RangeInclusive;
use std::str::FromStr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use async_std::prelude::*;
use async_std::{
    future,
    net::{Shutdown, TcpStream, ToSocketAddrs},
    sync::{Arc, Mutex},
    task,
};

use futures::channel::mpsc;
use futures::channel::oneshot;
use futures::SinkExt;
use futures::{stream, StreamExt};

use rand::distributions::{Alphanumeric, Distribution, Standard};
use rand::rngs::StdRng;
use rand::{Rng, SeedableRng};

use log::{debug, error, info, Level};

use super::oneshot_send_err;
use super::PeerConnection;
use super::{battle, Outcome, Weapon};
use super::{BoxErr, ResultBoxErr};
use super::{ParseSecsArgError, SecsArg};
use super::{ParseStatsError, Stats, UserName};

/* ************************************************************************* */
/* ************************************************************************* */

async fn log_termination<F>(fut: F)
where
    F: Future<Output = ResultBoxErr<()>>,
{
    super::log_termination_gen(fut, Level::Info, |_| Level::Error).await
}

/* ************************************************************************* */
/* ************************************************************************* */

impl Distribution<Weapon> for Standard {
    fn sample<R: Rng + ?Sized>(&self, rng: &mut R) -> Weapon {
        match rng.gen_range(0..=2) {
            0 => Weapon::Rock,
            1 => Weapon::Paper,
            2 => Weapon::Scissors,
            _ => unreachable!(),
        }
    }
}

/* ************************************************************************* */
/* ************************************************************************* */

#[derive(Debug, thiserror::Error)]
#[error("expected `{expected}`{} during {phase} phase, but got `{line}`",
        match xtra {None => Cow::Borrowed(""), Some(xtra) => Cow::Owned(format!(" with {xtra}"))})]
struct NextLineExpectError {
    expected: String,
    line: String,
    phase: &'static str,
    xtra: Option<String>,
}
#[derive(Debug, thiserror::Error)]
#[error("timeout after {:.2}s during {phase} phase", dur.as_secs_f32())]
struct NextLineTimeoutError {
    dur: Duration,
    phase: &'static str,
}
#[derive(Debug, thiserror::Error)]
#[error("expected `{expected}`{} during {phase} phase, but timeout after {:.2}s",
        match xtra {None => Cow::Borrowed(""), Some(xtra) => Cow::Owned(format!(" with {xtra}"))},
        dur.as_secs_f32())]
struct NextLineExpectTimeoutError {
    dur: Duration,
    expected: String,
    phase: &'static str,
    xtra: Option<String>,
}

/* ************************************************************************* */
/* ************************************************************************* */

impl PeerConnection {
    fn shutdown(&self) -> ResultBoxErr<()> {
        self.writer.shutdown(Shutdown::Both)?;
        Ok(())
    }

    async fn next_line_chk<F, T>(&mut self, phase: &'static str, pred: F) -> ResultBoxErr<T>
    where
        F: FnOnce(&str) -> Result<T, BoxErr>,
    {
        let line = self.next_line(phase).await?;
        let res = pred(&line)?;
        Ok(res)
    }
    async fn next_line_expect(&mut self, phase: &'static str, expect: &str) -> ResultBoxErr<()> {
        self.next_line_chk(phase, |line| {
            if line == expect {
                Ok(())
            } else {
                Err(NextLineExpectError {
                    expected: String::from(expect),
                    line: String::from(line),
                    phase,
                    xtra: None,
                }
                .into())
            }
        })
        .await
    }
    async fn next_line_timeout_err(
        &mut self,
        phase: &'static str,
        dur: Duration,
    ) -> ResultBoxErr<String> {
        match future::timeout(dur, self.next_line(phase)).await {
            Ok(res) => Ok(res?),
            Err(_) => Err(NextLineTimeoutError { dur, phase }.into()),
        }
    }
    async fn next_line_expect_timeout_err(
        &mut self,
        phase: &'static str,
        expect: &str,
        dur: Duration,
    ) -> ResultBoxErr<()> {
        match future::timeout(dur, self.next_line_expect(phase, expect)).await {
            Ok(res) => Ok(res?),
            Err(_) => Err(NextLineExpectTimeoutError {
                dur,
                expected: String::from(expect),
                phase,
                xtra: None,
            }
            .into()),
        }
    }
}

/* ************************************************************************* */
/* ************************************************************************* */

const SPAWN_SERVER_DEFAULT: bool = true;
const PLAYERS_DEFAULT: usize = 200;
const BADNESS_DEFAULT: f64 = 0.25;
const THROUGHPUT_INTERVAL_DEFAULT: OptSecsArg = OptSecsArg::None;
const CHECK_STANDINGS_INTERVAL_DEFAULT: OptSecsArg =
    OptSecsArg::Some(SecsArg(Duration::from_secs(0)));
const TESTING_TIME_DEFAULT: SecsArg = SecsArg(Duration::from_secs(300));
const EXCLUSIVE_PLAY_INTERVAL_DEFAULT: OptSecsArg =
    OptSecsArg::Some(SecsArg(Duration::from_secs(75)));
const TIMEOUT_SLOP_DEFAULT: SecsArg = SecsArg(Duration::from_secs(2));

#[derive(Debug, Clone, clap::Args)]
#[clap(next_help_heading = "Testing Options")]
pub struct Args {
    /// Spawn server as an async task
    #[clap(short = 'S', long, value_name = "BOOL", default_value_t = SPAWN_SERVER_DEFAULT, action = clap::ArgAction::Set, value_parser = str::parse::<bool>)]
    spawn_server: bool,
    /// Testing time (and then initiate shutdown)
    #[clap(short = 'T', long, value_name = "SECS", default_value_t = TESTING_TIME_DEFAULT)]
    testing_time: SecsArg,
    /// Number of players
    #[clap(short = 'P', long, value_name = "N", default_value_t = PLAYERS_DEFAULT)]
    players: usize,
    /// Fraction of bad (misbehaving) players
    #[clap(short = 'B', long, value_name = "FRAC", default_value_t = BADNESS_DEFAULT, value_parser = parse_frac)]
    bad_frac: f64,
    /// Interval at which to execute exclusive `play` command
    #[clap(short = 'E', long, value_name = "false|SECS", default_value_t = EXCLUSIVE_PLAY_INTERVAL_DEFAULT)]
    exclusive_play_interval: OptSecsArg,
    /// Interval at which to execute check-standings task
    #[clap(short = 'C', long, value_name = "false|SECS", default_value_t = CHECK_STANDINGS_INTERVAL_DEFAULT)]
    check_standings_interval: OptSecsArg,
    /// Interval at which to execute throughput task
    #[clap(short = 'I', long, value_name = "false|SECS", default_value_t = THROUGHPUT_INTERVAL_DEFAULT)]
    throughput_interval: OptSecsArg,
    /// Time to consider an "immediate" response to have timed out
    #[clap(long, value_name = "SECS", default_value_t = TIMEOUT_SLOP_DEFAULT)]
    timeout_slop: SecsArg,
}
const FRAC_RANGE: RangeInclusive<f64> = 0.0..=1.0;
#[derive(Debug, thiserror::Error)]
enum ParseFracArgError {
    #[error("{0:?}")]
    ParseFloatError(std::num::ParseFloatError),
    #[error("{}", format!(
        "not in range {:.1}-{:.1}",
        FRAC_RANGE.start(),
        FRAC_RANGE.end()
    ))]
    Range,
}
fn parse_frac(s: &str) -> Result<f64, ParseFracArgError> {
    match s.parse() {
        Ok(f) => {
            if FRAC_RANGE.contains(&f) {
                Ok(f)
            } else {
                Err(ParseFracArgError::Range)
            }
        }
        Err(err) => Err(ParseFracArgError::ParseFloatError(err)),
    }
}

#[derive(Debug, Clone)]
enum OptSecsArg {
    None,
    Some(super::SecsArg),
}
impl From<OptSecsArg> for Option<Duration> {
    fn from(s: OptSecsArg) -> Self {
        match s {
            OptSecsArg::None => None,
            OptSecsArg::Some(s) => Some(s.into()),
        }
    }
}
impl Display for OptSecsArg {
    fn fmt(&self, f: &mut Formatter) -> fmt::Result {
        match self {
            OptSecsArg::None => f.write_str("false"),
            OptSecsArg::Some(s) => write!(f, "{s}"),
        }
    }
}
impl FromStr for OptSecsArg {
    type Err = ParseSecsArgError;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        if s == "false" {
            Ok(OptSecsArg::None)
        } else {
            Ok(OptSecsArg::Some(s.parse()?))
        }
    }
}

/* ************************************************************************* */
/* ************************************************************************* */

pub async fn test(global_args: super::Args, test_args: Args) {
    async fn test(global_args: super::Args, test_args: Args) -> ResultBoxErr<()> {
        if test_args.spawn_server {
            let global_args = global_args.clone();
            let (ready_send, ready_recv) = oneshot::channel();
            let server = super::server(global_args, Some(ready_send));
            task::Builder::new()
                .name("server".to_string())
                .spawn(server)?;
            ready_recv.await?;
        }

        let hostname = global_args.hostname.as_str();
        let port = global_args.port;
        let command_timeout = global_args.command_timeout.into();
        let play_timeout = global_args.play_timeout.into();
        let weapon_timeout = global_args.weapon_timeout.into();

        let players = test_args.players;
        let bad_frac = test_args.bad_frac;
        let exclusive_interval: Option<Duration> = test_args.exclusive_play_interval.into();
        let check_standings_interval: Option<Duration> = test_args.check_standings_interval.into();
        let throughput_interval: Option<Duration> = test_args.throughput_interval.into();
        let testing_time = test_args.testing_time.into();
        let timeout_slop = test_args.timeout_slop.into();

        let addr = &format!("{hostname}:{port}");
        {
            let peer_connection = &mut connect(addr, timeout_slop).await?;
            let uname = "test";
            login(peer_connection, uname, None, command_timeout, timeout_slop).await?;
            command_prompt(peer_connection, command_timeout, timeout_slop).await?;
            quit_command(peer_connection, timeout_slop).await?;
        }

        let mut shutdowns_handles = Vec::new();

        let player_senders = &Arc::new(Mutex::new(HashMap::new()));
        let mut exclusive_opt =
            exclusive_interval.map(|exclusive_interval| (exclusive_interval, Vec::new()));
        let start = Instant::now();
        let games = &Arc::new(AtomicU64::new(0));
        for i in 1..=players {
            let addr = addr.to_owned();
            let uname = format!("player{i:04}");
            let (shutdown_send, shutdown_recv) = oneshot::channel();
            let bad = (i - 1) as f64 / players as f64 >= (1.0 - bad_frac);
            let mut rng = StdRng::seed_from_u64(i as u64);
            let (sender, mut receiver) = mpsc::channel(1);
            let players_senders = Arc::clone(player_senders);
            player_senders.lock().await.insert(uname.to_owned(), sender);
            let exclusive_recv = exclusive_opt.as_mut().map(|(_, player_exclusives)| {
                let (exclusive_send, exclusive_recv) = mpsc::channel(0);
                player_exclusives.push(exclusive_send);
                exclusive_recv
            });
            let games = Arc::clone(games);
            let player = {
                let uname = uname.clone();
                async move {
                    player(
                        &addr,
                        &uname,
                        shutdown_recv,
                        bad,
                        &mut rng,
                        &mut receiver,
                        &players_senders,
                        exclusive_recv,
                        &games,
                        command_timeout,
                        play_timeout,
                        weapon_timeout,
                        timeout_slop,
                    )
                    .await
                }
            };
            let player = log_termination(player);
            let handle = task::Builder::new()
                .name(format!("test:{uname}"))
                .spawn(player)?;
            shutdowns_handles.push((shutdown_send, handle))
        }

        match exclusive_opt {
            None => {}
            Some((exclusive_interval, player_exclusives)) => {
                let (shutdown_send, shutdown_recv) = oneshot::channel();
                let addr = addr.to_owned();
                let exclusive = async move {
                    exclusive(
                        &addr,
                        shutdown_recv,
                        exclusive_interval,
                        player_exclusives,
                        command_timeout,
                        play_timeout,
                        timeout_slop,
                    )
                    .await
                };
                let exclusive = log_termination(exclusive);
                let handle = task::Builder::new()
                    .name("test:exclusive".to_string())
                    .spawn(exclusive)?;
                shutdowns_handles.push((shutdown_send, handle))
            }
        };

        match check_standings_interval {
            None => {}
            Some(check_standings_interval) => {
                let (shutdown_send, shutdown_recv) = oneshot::channel();
                let addr = addr.to_owned();
                let check_standings = async move {
                    check_standings(
                        &addr,
                        shutdown_recv,
                        check_standings_interval,
                        command_timeout,
                        timeout_slop,
                    )
                    .await
                };
                let check_standings = log_termination(check_standings);
                let handle = task::Builder::new()
                    .name("test:check_standings".to_string())
                    .spawn(check_standings)?;
                shutdowns_handles.push((shutdown_send, handle))
            }
        };

        match throughput_interval {
            None => {}
            Some(throughput_interval) => {
                let throughput_name = "test:throughput";
                let (shutdown_send, shutdown_recv) = oneshot::channel();
                let games = Arc::clone(games);
                let throughput = async move {
                    let mut shutdown = shutdown_recv;
                    let games = &games;
                    loop {
                        let last = match future::timeout(throughput_interval, &mut shutdown).await {
                            Ok(res) => {
                                res?;
                                true
                            }
                            Err(_) => false,
                        };
                        let games = games.load(Ordering::Relaxed) as f64 / 2.0;
                        let time = (Instant::now() - start).as_secs_f64();
                        println!("{throughput_name}:: {:.2} games/sec", games / time);
                        if last {
                            return Ok(());
                        }
                    }
                };
                let throughput = log_termination(throughput);
                shutdowns_handles.push((
                    shutdown_send,
                    task::Builder::new()
                        .name(throughput_name.to_string())
                        .spawn(throughput)?,
                ));
            }
        };

        let shutdown_name = "test:shutdown";
        let shutdown = async move {
            task::sleep(testing_time).await;
            println!(
                "{shutdown_name}:: Initiating shutdown (an unmatched `play` command may require {:.2}s to timeout)",
                play_timeout.as_secs_f32()
            );
            stream::iter(shutdowns_handles)
                .for_each_concurrent(None, |(shutdown, handle)| async move {
                    if shutdown.send(()).is_err() {
                        let task = handle.task();
                        let name = task.name().unwrap_or("???");
                        error!("{shutdown_name}:: {name}'s shutdown channel cancelled")
                    };
                    handle.await
                })
                .await;
            println!("{shutdown_name}:: Finished shutdown");
            Ok(())
        };
        let shutdown = log_termination(shutdown);
        let shutdown_handle = task::Builder::new()
            .name(shutdown_name.to_string())
            .spawn(shutdown)?;
        shutdown_handle.await;
        Ok(())
    }

    log_termination(test(global_args, test_args)).await
}

/* ************************************************************************* */
/* ************************************************************************* */

async fn check_standings(
    addr: &str,
    mut shutdown: oneshot::Receiver<()>,
    check_standings_interval: Duration,
    command_timeout: Duration,
    timeout_slop: Duration,
) -> ResultBoxErr<()> {
    #[derive(Debug, thiserror::Error)]
    #[error("inconsistent standings (`{chk}` failed)")]
    struct StandingsError {
        chk: &'static str,
    }

    let uname = "checkstandings";
    let peer_connection = &mut connect(addr, timeout_slop).await?;
    login(peer_connection, uname, None, command_timeout, timeout_slop).await?;
    let mut next_check_standings = check_standings_interval;
    loop {
        command_prompt(peer_connection, command_timeout, timeout_slop).await?;
        let timeout = Duration::min(next_check_standings, command_timeout.mul_f32(0.9));
        match future::timeout(check_standings_interval, &mut shutdown).await {
            Ok(res) => {
                res?;
                quit_command(peer_connection, timeout_slop).await?;
                return Ok(());
            }
            Err(_) => {
                if next_check_standings > timeout {
                    next_check_standings -= timeout;
                    bogus_command(peer_connection, timeout_slop).await?;
                } else {
                    let standings = standings_command(peer_connection, timeout_slop).await?;
                    let mut tot_wins = 0;
                    let mut tot_losses = 0;
                    let mut tot_draws = 0;
                    let mut tot_forfeits = 0;
                    for stat in standings.values() {
                        tot_wins += stat.wins;
                        tot_losses += stat.losses;
                        tot_draws += stat.draws;
                        tot_forfeits += stat.forfeits;
                    }
                    macro_rules! chk {
                        ($e:expr) => {
                            if !($e) {
                                return Err(StandingsError {
                                    chk: stringify!($e),
                                }
                                .into());
                            }
                        };
                    }
                    chk!((tot_wins + tot_draws + tot_losses + tot_forfeits) % 2 == 0);
                    chk!(tot_draws % 2 == 0);
                    chk!((tot_wins + tot_losses + tot_forfeits) % 2 == 0);
                    chk!(tot_wins >= tot_losses);
                    chk!((tot_losses + tot_forfeits) >= tot_wins);
                    chk!((tot_losses + tot_forfeits - tot_wins) % 2 == 0);
                }
            }
        }
    }
}

/* ************************************************************************* */
/* ************************************************************************* */

async fn connect<A: ToSocketAddrs>(
    addrs: A,
    timeout_slop: Duration,
) -> ResultBoxErr<PeerConnection> {
    let stream = TcpStream::connect(addrs).await?;
    let mut peer_connection = PeerConnection::new(stream)?;

    peer_connection
        .next_line_expect_timeout_err("welcome", "Welcome to ROCK-PAPER-SCISSORS!", timeout_slop)
        .await?;
    peer_connection
        .next_line_expect_timeout_err("welcome", "", timeout_slop)
        .await?;
    Ok(peer_connection)
}

async fn reconnect(
    peer_connection: &mut PeerConnection,
    uname: &str,
    sleep_time: Duration,
    command_timeout: Duration,
    timeout_slop: Duration,
) -> ResultBoxErr<()> {
    let peer_addr = peer_connection.peer_addr();
    peer_connection.shutdown()?;
    task::sleep(sleep_time).await;
    *peer_connection = connect(peer_addr, timeout_slop).await?;
    login(peer_connection, uname, None, command_timeout, timeout_slop).await?;
    Ok(())
}

enum LoginKind {
    NewUser,
    RetUser,
}

async fn login(
    peer_connection: &mut PeerConnection,
    uname: &str,
    login_kind: Option<LoginKind>,
    command_timeout: Duration,
    timeout_slop: Duration,
) -> ResultBoxErr<()> {
    let passwd = uname;

    peer_connection
        .next_line_expect_timeout_err(
            "login",
            &format!(
                "Enter username [{:.2}s timeout]:",
                command_timeout.as_secs_f32()
            ),
            timeout_slop,
        )
        .await?;
    peer_connection.writeln_str(uname, "login").await?;

    let new_user_line = "";
    let ret_user_line = format!(
        "Enter password [{:.2}s timeout]:",
        command_timeout.as_secs_f32()
    );
    let ret_user_line = ret_user_line.as_str();
    let login_kind = match login_kind {
        Some(LoginKind::NewUser) => {
            peer_connection
                .next_line_expect_timeout_err("login", new_user_line, timeout_slop)
                .await?;
            LoginKind::NewUser
        }
        Some(LoginKind::RetUser) => {
            peer_connection
                .next_line_expect_timeout_err("login", ret_user_line, timeout_slop)
                .await?;
            LoginKind::RetUser
        }
        None => {
            let line = peer_connection
                .next_line_timeout_err("login", timeout_slop)
                .await?;
            if line == new_user_line {
                LoginKind::NewUser
            } else if line == ret_user_line {
                LoginKind::RetUser
            } else {
                return Err(NextLineExpectError {
                    expected: format!("{new_user_line}` or `{ret_user_line}"),
                    line,
                    phase: "login",
                    xtra: None,
                }
                .into());
            }
        }
    };
    match login_kind {
        LoginKind::NewUser => {
            peer_connection
                .next_line_expect_timeout_err("login", "Welcome new user!", timeout_slop)
                .await?;
            peer_connection
                .next_line_expect_timeout_err("login", "", timeout_slop)
                .await?;
            peer_connection
                .next_line_expect_timeout_err(
                    "login",
                    &format!(
                        "Enter initial password [{:.2}s timeout]:",
                        command_timeout.as_secs_f32()
                    ),
                    timeout_slop,
                )
                .await?;
        }
        LoginKind::RetUser => {}
    };

    peer_connection.writeln_str(passwd, "password").await?;
    peer_connection
        .next_line_expect_timeout_err("login", "", timeout_slop)
        .await?;
    Ok(())
}

async fn command_prompt(
    peer_connection: &mut PeerConnection,
    command_timeout: Duration,
    timeout_slop: Duration,
) -> ResultBoxErr<()> {
    peer_connection
        .next_line_expect_timeout_err(
            "command",
            &format!(
                "Enter command {{passwd,play,players,standings,stats,quit}} [{:.2}s timeout]:",
                command_timeout.as_secs_f32(),
            ),
            timeout_slop,
        )
        .await?;
    Ok(())
}

fn parse_stats_line(line: &str) -> Result<(&str, Stats), ParseStatsError> {
    let err = ParseStatsError;
    let mut iter = line.splitn(2, ": ");
    let uname = iter.next().ok_or(err)?;
    let stats = iter.next().ok_or(err)?.parse()?;
    if iter.next().is_some() {
        return Err(err);
    }
    Ok((uname, stats))
}

async fn stats_command(
    peer_connection: &mut PeerConnection,
    expected_uname: &str,
    timeout_slop: Duration,
) -> ResultBoxErr<Stats> {
    peer_connection.writeln_str("stats", "command").await?;
    peer_connection
        .next_line_expect_timeout_err("stats", "", timeout_slop)
        .await?;
    let line = peer_connection
        .next_line_timeout_err("stats", timeout_slop)
        .await?;
    let err = || {
        NextLineExpectError {
            expected: format!("{expected_uname}: <stats>"),
            line: line.clone(),
            phase: "stats",
            xtra: None,
        }
        .into()
    };
    let (uname, stats) = parse_stats_line(&line).map_err(|_| err())?;
    if uname != expected_uname {
        return Err(err());
    }
    peer_connection
        .next_line_expect_timeout_err("stats", "", timeout_slop)
        .await?;
    Ok(stats)
}

async fn standings_command(
    peer_connection: &mut PeerConnection,
    timeout_slop: Duration,
) -> ResultBoxErr<HashMap<UserName, Stats>> {
    peer_connection.writeln_str("standings", "command").await?;
    peer_connection
        .next_line_expect_timeout_err("standings", "", timeout_slop)
        .await?;
    let mut standings = HashMap::new();
    loop {
        let line = peer_connection
            .next_line_timeout_err("standings", timeout_slop)
            .await?;
        if line.is_empty() {
            break;
        }
        let err = || NextLineExpectError {
            expected: String::from("<n>. <uname>: <stats>"),
            line: line.clone(),
            phase: "stats",
            xtra: None,
        };
        let line = line.trim_start_matches(|c: char| c.is_ascii_digit());
        let mut first = true;
        let line = line
            .strip_prefix(|c| {
                first && {
                    first = false;
                    c == '.'
                }
            })
            .ok_or_else(err)?;
        let line = line.trim_start();
        let (uname, stats) = parse_stats_line(line).map_err(|_| err())?;
        standings.insert(uname.to_owned(), stats);
    }
    Ok(standings)
}

async fn quit_command(
    peer_connection: &mut PeerConnection,
    timeout_slop: Duration,
) -> ResultBoxErr<()> {
    peer_connection.writeln_str("quit", "command").await?;
    peer_connection
        .next_line_expect_timeout_err("quit", "Goodbye!", timeout_slop)
        .await?;
    Ok(())
}

async fn bogus_command(
    peer_connection: &mut PeerConnection,
    timeout_slop: Duration,
) -> ResultBoxErr<()> {
    peer_connection.writeln_str("bogus", "command").await?;
    peer_connection
        .next_line_expect_timeout_err("bogus", "Invalid command (`bogus`)", timeout_slop)
        .await?;
    peer_connection
        .next_line_expect_timeout_err("bogus", "", timeout_slop)
        .await?;
    Ok(())
}

#[allow(clippy::too_many_arguments)]
async fn player<R: Rng>(
    addr: &str,
    uname: &str,
    mut shutdown: oneshot::Receiver<()>,
    bad: bool,
    rng: &mut R,
    receiver: &mut mpsc::Receiver<OpponentMsg>,
    players_senders: &Mutex<HashMap<UserName, mpsc::Sender<OpponentMsg>>>,
    mut exclusive: Option<mpsc::Receiver<oneshot::Sender<oneshot::Sender<()>>>>,
    games: &AtomicU64,
    command_timeout: Duration,
    play_timeout: Duration,
    weapon_timeout: Duration,
    timeout_slop: Duration,
) -> ResultBoxErr<()> {
    let peer_connection = &mut connect(addr, timeout_slop).await?;
    login(peer_connection, uname, None, command_timeout, timeout_slop).await?;
    command_prompt(peer_connection, command_timeout, timeout_slop).await?;
    let mut stats = stats_command(peer_connection, uname, timeout_slop).await?;
    'command: loop {
        command_prompt(peer_connection, command_timeout, timeout_slop).await?;
        match shutdown.try_recv()? {
            None => {}
            Some(()) => {
                quit_command(peer_connection, timeout_slop).await?;
                return Ok(());
            }
        }
        if let Some(exclusive) = &mut exclusive {
            match exclusive.try_next() {
                Ok(None) => {
                    /* exclusive task has shutdown */
                    bogus_command(peer_connection, timeout_slop).await?;
                    continue 'command;
                }
                Ok(Some(ack_send)) => {
                    debug!("test:{uname}:: Waiting for exclusive `play`");
                    let (resume_send, mut resume_recv) = oneshot::channel();
                    ack_send.send(resume_send).map_err(oneshot_send_err)?;
                    loop {
                        match future::timeout(command_timeout.mul_f32(0.9), &mut resume_recv).await
                        {
                            Ok(Ok(())) => break,
                            Ok(Err(err)) => return Err(err.into()),
                            Err(_) => {
                                bogus_command(peer_connection, timeout_slop).await?;
                                command_prompt(peer_connection, command_timeout, timeout_slop)
                                    .await?;
                            }
                        }
                    }
                    debug!("test:{uname}:: Resuming after exclusive `play`");
                    bogus_command(peer_connection, timeout_slop).await?;
                    continue 'command;
                }
                Err(_) => {}
            }
        }
        let outcome = player_play(
            peer_connection,
            uname,
            bad,
            rng,
            receiver,
            players_senders,
            games,
            command_timeout,
            play_timeout,
            weapon_timeout,
            timeout_slop,
        )
        .await?;
        command_prompt(peer_connection, command_timeout, timeout_slop).await?;
        let next_stats = stats_command(peer_connection, uname, timeout_slop).await?;
        let inc_stats = |outcome| {
            let mut stats = stats;
            stats.inc_by_outcome(outcome);
            stats
        };
        let check_next_stats = |maybe_statss: &[Stats]| -> ResultBoxErr<()> {
            #[derive(Debug, thiserror::Error)]
            #[error("inconsistent stats (expected `{expected}`, but got `{actual}`")]
            struct InconsistentStatsError {
                expected: String,
                actual: String,
            }
            match maybe_statss
                .iter()
                .find(|maybe_stats| &next_stats == *maybe_stats)
            {
                None => Err(InconsistentStatsError {
                    actual: next_stats.to_string(),
                    expected: match maybe_statss {
                        [maybe_stats] => format!("{maybe_stats}"),
                        [maybe_stats0, maybe_stats1] => {
                            format!("{maybe_stats0}` or `{maybe_stats1}")
                        }
                        _ => "???".to_string(),
                    },
                }
                .into()),
                Some(_) => Ok(()),
            }
        };
        match outcome {
            PlayOutcome::NoOpponent => check_next_stats(&[stats])?,
            PlayOutcome::NoOpponentOrForfeit => {
                check_next_stats(&[stats, inc_stats(Outcome::Forfeit)])?
            }
            PlayOutcome::Battle(outcome) => check_next_stats(&[inc_stats(outcome)])?,
        };
        stats = next_stats;
    }
}

type OpponentMsg = (UserName, bool, Option<Weapon>);

enum PlayOutcome {
    NoOpponent,
    NoOpponentOrForfeit,
    Battle(Outcome),
}

#[allow(clippy::too_many_arguments)]
async fn player_play<R: Rng>(
    peer_connection: &mut PeerConnection,
    uname: &str,
    bad: bool,
    rng: &mut R,
    receiver: &mut mpsc::Receiver<OpponentMsg>,
    players_senders: &Mutex<HashMap<UserName, mpsc::Sender<OpponentMsg>>>,
    games: &AtomicU64,
    command_timeout: Duration,
    play_timeout: Duration,
    weapon_timeout: Duration,
    timeout_slop: Duration,
) -> ResultBoxErr<PlayOutcome> {
    let _ = receiver.try_next();

    let mut misbehaves = [false; 10];
    if bad {
        misbehaves[rng.gen_range(0..misbehaves.len())] = true;
    }

    macro_rules! reconnect {
        ($idx:literal , $timeout:expr , $outcome:expr) => {
            if misbehaves[$idx] {
                reconnect(
                    peer_connection,
                    uname,
                    $timeout,
                    command_timeout,
                    timeout_slop,
                )
                .await?;
                return Ok($outcome);
            }
        };
    }

    peer_connection.writeln_str("play", "command").await?;

    reconnect!(0, play_timeout, PlayOutcome::NoOpponentOrForfeit);

    peer_connection
        .next_line_expect_timeout_err("play", "", timeout_slop.mul_f32(1.25))
        .await?;

    reconnect!(1, play_timeout, PlayOutcome::NoOpponentOrForfeit);

    peer_connection
        .next_line_expect_timeout_err(
            "play",
            &format!(
                "Waiting for opponent [{:.2}s timeout]...",
                play_timeout.as_secs_f32()
            ),
            timeout_slop.mul_f32(1.25),
        )
        .await?;

    reconnect!(2, play_timeout, PlayOutcome::NoOpponentOrForfeit);

    let no_opponent_msg = "Sorry, no opponents are ready to battle.";
    let opponent_msg_prefix = &format!("{uname} versus ");
    let opponent_msg_suffix = "!!";
    let opponent_msg = &format!("{opponent_msg_prefix}<opponent>{opponent_msg_suffix}");
    let opponent_timeout = play_timeout.mul_f32(1.25);
    let line;
    let opponent = match future::timeout(opponent_timeout, peer_connection.next_line("play")).await
    {
        Ok(line_) => {
            line = line_?;
            if line == no_opponent_msg {
                peer_connection
                    .next_line_expect_timeout_err("play", "", timeout_slop.mul_f32(1.25))
                    .await?;
                return Ok(PlayOutcome::NoOpponent);
            } else {
                match line
                    .strip_prefix(opponent_msg_prefix)
                    .and_then(|line| line.strip_suffix(opponent_msg_suffix))
                {
                    None => {
                        return Err(NextLineExpectError {
                            expected: format!("{no_opponent_msg}` or `{opponent_msg}"),
                            line,
                            phase: "play",
                            xtra: None,
                        }
                        .into());
                    }
                    Some(opponent) => opponent,
                }
            }
        }
        Err(_) => {
            return Err(NextLineExpectTimeoutError {
                dur: opponent_timeout,
                expected: format!("{no_opponent_msg}` or `{opponent_msg}"),
                phase: "play",
                xtra: None,
            }
            .into());
        }
    };

    reconnect!(3, weapon_timeout, PlayOutcome::Battle(Outcome::Forfeit));

    #[allow(non_snake_case)]
    let MIN_WEAPON_SLEEP: Duration = Duration::from_secs_f32(0.5);
    let mut prev_wtimeout = weapon_timeout;
    let weapon: Option<Weapon> = {
        let weapon_prefix = "Choose your weapon {r,p,s} [";
        let weapon_suffix = "s timeout]:";
        let weapon_msg = &format!("{weapon_prefix}<wtimeout>{weapon_suffix}");
        loop {
            let line = peer_connection
                .next_line_timeout_err("play", timeout_slop.mul_f32(1.25))
                .await?;
            let mk_weapon_err = |xtra: Option<&str>| {
                Err(NextLineExpectError {
                    expected: weapon_msg.to_string(),
                    phase: "play",
                    line: line.clone(),
                    xtra: xtra.map(String::from),
                }
                .into())
            };
            let wtimeout = match line
                .strip_prefix(weapon_prefix)
                .and_then(|line| line.strip_suffix(weapon_suffix))
                .and_then(|wtimeout| wtimeout.parse::<f64>().ok())
            {
                None => return mk_weapon_err(None),
                Some(wtimeout) => {
                    let wtimeout = Duration::from_secs_f64(wtimeout);
                    #[allow(clippy::collapsible_else_if)]
                    if prev_wtimeout <= MIN_WEAPON_SLEEP || prev_wtimeout == weapon_timeout {
                        if wtimeout > prev_wtimeout {
                            return mk_weapon_err(Some(&format!(
                                "<wtimeout>s <= {:.2}s failed",
                                prev_wtimeout.as_secs_f32()
                            )));
                        }
                    } else {
                        if wtimeout >= prev_wtimeout {
                            return mk_weapon_err(Some(&format!(
                                "<wtimeout>s < {:.2}s failed",
                                prev_wtimeout.as_secs_f32()
                            )));
                        }
                    }
                    prev_wtimeout = wtimeout;
                    wtimeout
                }
            };
            let mut weapon_misbehaves = [0u32; 2];
            if misbehaves[7] || misbehaves[8] {
                weapon_misbehaves[rng.gen_range(0..weapon_misbehaves.len())] = rng.gen_range(0..16);
            }
            if weapon_misbehaves[0] > 10 || wtimeout <= timeout_slop {
                let weapon_timeout_timeout = wtimeout + timeout_slop;
                let weapon_timeout_prefix = "Timeout after ";
                let weapon_timeout_suffix = "s";
                let weapon_timeout_msg = &format!(
                    "{weapon_timeout_prefix}{:.2}{weapon_timeout_suffix}",
                    wtimeout.as_secs_f32(),
                );
                match future::timeout(weapon_timeout_timeout, peer_connection.next_line("weapon"))
                    .await
                {
                    Ok(line) => {
                        let line = line?;
                        let mk_weapon_timeout_err = |xtra: Option<&str>| {
                            Err(NextLineExpectError {
                                expected: weapon_timeout_msg.to_string(),
                                line: line.clone(),
                                phase: "weapon",
                                xtra: xtra.map(String::from),
                            }
                            .into())
                        };
                        match line
                            .strip_prefix(weapon_timeout_prefix)
                            .and_then(|line| line.strip_suffix(weapon_timeout_suffix))
                            .and_then(|weapon_timeout_timeout| {
                                weapon_timeout_timeout.parse::<f64>().ok()
                            }) {
                            None => return mk_weapon_timeout_err(None),
                            Some(weapon_timeout_timeout) => {
                                let weapon_timeout_timeout =
                                    Duration::from_secs_f64(weapon_timeout_timeout);
                                if weapon_timeout_timeout != wtimeout {
                                    return mk_weapon_timeout_err(None);
                                } else {
                                    break None;
                                }
                            }
                        }
                    }
                    Err(_) => {
                        return Err(NextLineExpectTimeoutError {
                            dur: weapon_timeout_timeout,
                            expected: String::from(weapon_timeout_msg),
                            phase: "weapon",
                            xtra: None,
                        }
                        .into())
                    }
                }
            };
            if weapon_misbehaves[1] > 1 {
                if wtimeout.mul_f32(0.75) > MIN_WEAPON_SLEEP {
                    let sleep = rng.gen_range(MIN_WEAPON_SLEEP..wtimeout.mul_f32(0.75));
                    task::sleep(sleep).await;
                    prev_wtimeout -= MIN_WEAPON_SLEEP.mul_f32(0.75);
                }
                let input = match rng.gen_range(0..3) {
                    0 => rng.gen::<Weapon>().to_string(),
                    1 => {
                        let input = match rng.gen_range(0..3) {
                            0 => "✊",
                            1 => "✋",
                            2 => "✌",
                            _ => "",
                        };
                        input.to_owned()
                    }
                    _ => loop {
                        let input = (0..rng.gen_range(0..30))
                            .map(|_| {
                                if rng.gen_bool(0.95) {
                                    rng.sample(Alphanumeric).into()
                                } else {
                                    ' '
                                }
                            })
                            .collect::<String>()
                            .trim()
                            .to_owned();
                        if ["r", "p", "s"].iter().any(|&w| input == w) {
                            continue;
                        } else {
                            break input;
                        }
                    },
                };
                peer_connection.writeln_str(&input, "weapon").await?;
                if weapon_misbehaves[1] == 14 {
                    reconnect(
                        peer_connection,
                        uname,
                        weapon_timeout,
                        command_timeout,
                        timeout_slop,
                    )
                    .await?;
                    return Ok(PlayOutcome::Battle(Outcome::Forfeit));
                }
                peer_connection
                    .next_line_expect_timeout_err(
                        "weapon",
                        &format!("Invalid weapon (`{input}`)"),
                        timeout_slop.mul_f32(1.25),
                    )
                    .await?;
                if weapon_misbehaves[1] == 15 {
                    reconnect(
                        peer_connection,
                        uname,
                        weapon_timeout,
                        command_timeout,
                        timeout_slop,
                    )
                    .await?;
                    return Ok(PlayOutcome::Battle(Outcome::Forfeit));
                }
            } else {
                let weapon: Weapon = rng.gen();
                peer_connection
                    .writeln_str(&weapon.to_string()[0..1], "weapon")
                    .await?;
                break Some(weapon);
            }
        }
    };

    #[derive(Debug, thiserror::Error)]
    #[error("{msg}")]
    struct OpponentError {
        msg: String,
    }

    players_senders
        .lock()
        .await
        .get_mut(opponent)
        .ok_or_else(|| OpponentError {
            msg: format!("unknown opponent `{opponent}` during play phase"),
        })?
        .send((uname.to_owned(), bad, weapon))
        .await
        .map_err(|_| OpponentError {
            msg: format!("opponent `{opponent}` unexpectedly terminated during play phase"),
        })?;

    let (opponent_weapon, opponent_bad) =
        match future::timeout(weapon_timeout.mul_f32(1.25), receiver.next()).await {
            Ok(None) => (None, true),
            Ok(Some((msg_opponent, msg_opponent_bad, msg_opponent_weapon))) => {
                if msg_opponent != opponent {
                    return Err(OpponentError {
                        msg: format!(
                        "expected opponent `{opponent}` but got `{msg_opponent}` during play phase"
                    ),
                    }
                    .into());
                }
                (msg_opponent_weapon, msg_opponent_bad)
            }
            Err(_) => (None, true),
        };
    let (outcome, _) = battle(weapon, opponent_weapon);

    reconnect!(4, weapon_timeout, PlayOutcome::Battle(outcome));

    peer_connection
        .next_line_expect_timeout_err(
            "play",
            &format!(
                "{opponent} {}; you {outcome}!",
                match opponent_weapon {
                    None => Cow::Borrowed("forfeits"),
                    Some(opponent_weapon) => Cow::Owned(format!("plays {opponent_weapon}")),
                },
            ),
            timeout_slop.mul_f32(1.25),
        )
        .await?;

    reconnect!(5, weapon_timeout, PlayOutcome::Battle(outcome));

    peer_connection
        .next_line_expect_timeout_err("play", "", timeout_slop.mul_f32(1.25))
        .await?;

    reconnect!(6, weapon_timeout, PlayOutcome::Battle(outcome));

    if !bad && !opponent_bad {
        games.fetch_add(1, Ordering::Relaxed);
    }
    Ok(PlayOutcome::Battle(outcome))
}

#[allow(clippy::too_many_arguments)]
async fn exclusive(
    addr: &str,
    mut shutdown: oneshot::Receiver<()>,
    exclusive_interval: Duration,
    mut player_exclusives: Vec<mpsc::Sender<oneshot::Sender<oneshot::Sender<()>>>>,
    command_timeout: Duration,
    play_timeout: Duration,
    timeout_slop: Duration,
) -> ResultBoxErr<()> {
    let uname = "exclusive";
    let peer_connection = &mut connect(addr, timeout_slop).await?;
    login(peer_connection, uname, None, command_timeout, timeout_slop).await?;
    let mut next_exclusive = exclusive_interval;
    'command: loop {
        command_prompt(peer_connection, command_timeout, timeout_slop).await?;
        let timeout = Duration::min(next_exclusive, command_timeout.mul_f32(0.9));
        match future::timeout(timeout, &mut shutdown).await {
            Ok(res) => {
                res?;
                quit_command(peer_connection, timeout_slop).await?;
                return Ok(());
            }
            Err(_) => {
                if next_exclusive > timeout {
                    next_exclusive -= timeout;
                    bogus_command(peer_connection, timeout_slop).await?;
                } else {
                    bogus_command(peer_connection, timeout_slop).await?;
                    command_prompt(peer_connection, command_timeout, timeout_slop).await?;
                    info!("test:{uname}:: Requesting exclusive `play`");
                    let mut exclusive = Box::pin(async {
                        let mut ack_recvs = Vec::new();
                        for exclusive in player_exclusives.iter_mut() {
                            let (ack_send, ack_recv) = oneshot::channel();
                            match exclusive.send(ack_send).await {
                                Ok(()) => ack_recvs.push(ack_recv),
                                Err(_) => {
                                    /* player task has terminated */
                                    for ack_recv in ack_recvs.into_iter() {
                                        #[allow(clippy::single_match)]
                                        match ack_recv.await {
                                            Ok(resume_send) => {
                                                resume_send.send(()).map_err(oneshot_send_err)?
                                            }
                                            Err(_) => {
                                                /* player task has terminated, after exclusive task sent
                                                 * ack_send, but before player task received ack_send.
                                                 */
                                            }
                                        }
                                    }
                                    return Ok(None);
                                }
                            }
                        }
                        let mut abort = false;
                        let mut resume_sends = Vec::new();
                        for ack_recv in ack_recvs.into_iter() {
                            match ack_recv.await {
                                Ok(resume_send) => resume_sends.push(resume_send),
                                Err(_) => {
                                    /* player task has terminated, after
                                     * exclusive task sent ack_send, but before
                                     * player task received ack_send. */
                                    abort = true
                                }
                            }
                        }
                        if abort {
                            for resume_send in resume_sends.into_iter() {
                                resume_send.send(()).map_err(oneshot_send_err)?
                            }
                            return Ok(None);
                        }
                        Ok::<_, BoxErr>(Some(resume_sends))
                    });
                    let resume_sends = loop {
                        match future::timeout(command_timeout.mul_f32(0.9), &mut exclusive).await {
                            Ok(Ok(Some(resume_sends))) => break resume_sends,
                            Ok(Ok(None)) => {
                                info!("test:{uname}:: Aborted exclusive `play`");
                                bogus_command(peer_connection, timeout_slop).await?;
                                continue 'command;
                            }
                            Ok(Err(err)) => return Err(err),
                            Err(_) => {
                                bogus_command(peer_connection, timeout_slop).await?;
                                command_prompt(peer_connection, command_timeout, timeout_slop)
                                    .await?;
                            }
                        }
                    };
                    info!("test:{uname}:: Starting exclusive `play`");
                    peer_connection.writeln_str("play", "command").await?;
                    peer_connection
                        .next_line_expect_timeout_err("play", "", timeout_slop)
                        .await?;
                    peer_connection
                        .next_line_expect_timeout_err(
                            "play",
                            &format!(
                                "Waiting for opponent [{:.2}s timeout]...",
                                play_timeout.as_secs_f32()
                            ),
                            timeout_slop,
                        )
                        .await?;
                    peer_connection
                        .next_line_expect_timeout_err(
                            "play",
                            "Sorry, no opponents are ready to battle.",
                            play_timeout + timeout_slop,
                        )
                        .await?;
                    peer_connection
                        .next_line_expect_timeout_err("play", "", timeout_slop)
                        .await?;
                    info!("test:{uname}:: Finished exclusive `play`");
                    for resume_send in resume_sends {
                        resume_send.send(()).map_err(oneshot_send_err)?
                    }
                    next_exclusive = exclusive_interval;
                }
            }
        }
    }
}
