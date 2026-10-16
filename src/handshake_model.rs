//! Модель рукопожатия single-client канала (Task 22, §6 и §7.1 п. 17
//! дизайна): исчерпывающий перебор всех чередований атомарных шагов двух
//! клиентов и сервера -- детерминированное доказательство на std, без
//! потоков.
//!
//! Модель -- тот же протокол, что в `client.rs`/`server.rs`, на уровне
//! отдельных обращений к разделяемым словам и событиям (каждое обращение --
//! один переход): `slot_claim` (токен уникален на каждую попытку, как у
//! `claim_token`), `client_state` (с «призрачной» пометкой, чья это
//! заявка/сессия), `generation`, `server_state`, `client_pid`, события
//! `CONNECT_REQ`, `S2C_CONNECT`, `DISCONNECT` (автосброс: ожидание
//! потребляет сигнал; у `DISCONNECT` -- призрак «кто взвёл»). Клиент делает
//! до `ATTEMPTS` попыток подключения; на любом шаге ожидания возможен
//! таймаут (W); подключённый клиент уходит (L) или разбирает `DISCONNECT`
//! сервера (worker Auto: любой сигнал -- конец сессии). Сервер принимает
//! заявки, отключает клиента сам (не больше одного раза за прогон) и
//! разбирает уход клиента.
//!
//! Проверяемые свойства (§6 дизайна):
//! - **I2** -- `client_state == HELLO/SERVER_READY` только у текущего
//!   держателя claim (заявка/сессия одна и принадлежит ему);
//! - **I4** -- клиент засчитывает подключение только к сессии, которую
//!   сервер зафиксировал на ЕГО заявке, и каждую сессию -- не больше одного
//!   клиента; сервер наблюдает процесс именно этого клиента, а самоописание
//!   (`client_pid`) владельца заявки к S1 не потеряно;
//! - **I5** -- уход клиента (CAS `SERVER_READY -> IDLE`, `DISCONNECT`)
//!   задевает только его собственную сессию;
//! - **G1** -- `DISCONNECT`, разобранный стороной сессии, взвёл её пир этой
//!   же сессии (сигнал не теряется и не достаётся чужой сессии).
//!
//! Допущения (как в дизайне): A1 -- держатель проходит C1..C6 быстрее
//! `RESERVE_TIMEOUT`, поэтому отъём протухшего захвата моделируется только
//! у клиента, который «не заметил» отключения сервером (он «забыт»: его
//! поздний уход -- один переход, наносекунды против 10-секундного отъёма и
//! целого рукопожатия следующего клиента, §6 «Остаточный риск»). Смерти
//! процессов (S2, освобождение claim умершего) -- в unit-тестах
//! `server.rs`, не в модели. PID клиента в модели -- его номер (разные
//! клиенты -- разные процессы; совпадение PID одного процесса моделирует
//! повторная попытка того же клиента).
//!
//! Варианты `DesignC10`/`DesignS6` -- ревизия 3 буквально по дизайну в одном
//! из двух мест, где модель нашла пробел (отклонения §2.1 C10 и §2.2 S6):
//! перебор обязан найти нарушение -- тест фиксирует, что отклонения нужны.
//!
//! Вариант `Revision::Two` -- протокол ревизии 2 (store `HELLO`, без claim,
//! принятие по смене `generation`, уход без сверки, сброс `DISCONNECT`
//! после фиксации): модель обязана найти нарушение (Concern 1 ревью
//! Task 18) -- это проверка зубов модели.

use std::collections::{HashMap, VecDeque};

/// Попыток подключения на клиента.
const ATTEMPTS: u8 = 2;
/// Клиенты: 1 и 2 (`0` -- «никто»: свободный claim, нулевой PID).
const CLIENTS: usize = 2;
/// Метка «`DISCONNECT` взвёл сервер».
const SERVER: u8 = 3;
/// Предел `generation` в модели (рукопожатий за прогон не больше 2 × 2).
const GENERATIONS: usize = 8;

/// Вариант протокола.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Revision {
    /// Ревизия 2 (до Task 22).
    Two,
    /// Ревизия 3 буквально по дизайну, но C10 -- «`SERVER_READY` или
    /// `generation != g0`» (без отклонения §2.1 C10).
    DesignC10,
    /// Ревизия 3 буквально по дизайну, но S6 -- `swap` без сверки (без
    /// отклонения §2.2 S6).
    DesignS6,
    /// Ревизия 3 с обоими отклонениями -- реализованный протокол.
    Three,
}

/// Значение `client_state` с призрачной пометкой: чья заявка/сессия.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
enum Cs {
    Idle,
    Hello(u8),
    Ready(u8),
}

/// Шаг клиента (имена -- шаги §2.1 дизайна).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
enum Pc {
    /// C1 (новая попытка).
    Start,
    C3,
    C4,
    C5,
    C6,
    C8,
    C9,
    /// C10: ожидание `S2C_CONNECT` (или таймаут -> W).
    Wait,
    /// Отзыв W.
    Withdraw,
    /// Откат: `client_pid = 0`.
    RollbackPid,
    /// Откат: CAS `T -> 0`.
    Release,
    /// Шаг A: принят.
    Accept,
    /// Подключён.
    Conn,
    /// L: сверка `generation`.
    L1,
    /// L: CAS `SERVER_READY -> IDLE`.
    L2,
    /// L: `DISCONNECT`.
    L3,
    /// L: CAS `T -> 0` (последним).
    L4,
    /// Claim отнят отъёмом, пока клиент «не заметил» отключения.
    Forgotten,
    Done,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
struct Client {
    pc: Pc,
    attempts: u8,
    g0: u8,
    g: u8,
    /// Подключение засчитано по C8-провалу: уйти сразу (L).
    leave_now: bool,
}

/// Шаг сервера (§2.2 дизайна; S2 -- смерти -- вне модели).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
enum Spc {
    Idle,
    S1,
    S3,
    S4,
    S5,
    /// S6: прочитать `slot_claim` (`t2`).
    S6,
    /// S6: прочитать самоописание (`client_pid`).
    S6Pid,
    /// S6: сверить `client_state == SERVER_READY` (владелец ещё в сессии).
    S6Check,
    /// S6: потребить самоописание CAS-ом `pid2 -> 0`.
    S6Consume,
    /// S6: claim всё ещё `t2` -- потреблено самоописание владельца; иначе
    /// вернуть (`CAS 0 -> pid2`).
    S6Verify,
    S7,
    Conn,
    /// Отключение по инициативе сервера: `DISCONNECT` после `IDLE`.
    KickSignal,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
struct Server {
    pc: Spc,
    /// Claim и PID, прочитанные в S1.
    t: u8,
    pid: u8,
    /// Claim и самоописание, прочитанные в S6.
    t2: u8,
    pid2: u8,
    /// S6 застал владельца в сессии (самоописание и claim -- его).
    present: bool,
    /// Наблюдаемый процесс и claim сессии.
    peer: u8,
    sclaim: u8,
    /// Призрак: чья заявка зафиксирована в текущей сессии.
    owner: u8,
    connected: bool,
    kicked: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
struct State {
    claim: u8,
    cs: Cs,
    generation: u8,
    server_ready: bool,
    client_pid: u8,
    req: bool,
    ack: bool,
    /// `DISCONNECT` и призрак -- кто его взвёл: `0` -- не взведён, клиент
    /// `1`/`2`, `SERVER` -- сервер.
    disconnect: u8,
    clients: [Client; CLIENTS],
    server: Server,
    /// Призрак: владелец сессии каждого `generation` (0 -- не было).
    commits: [u8; GENERATIONS],
    /// Призрак: кто засчитал подключение к сессии каждого `generation`.
    accepted: [u8; GENERATIONS],
}

/// Итог перебора.
#[derive(Debug)]
struct Stats {
    states: usize,
    accepts: bool,
}

/// Токен попытки: уникален на (клиент, попытку), как у `claim_token`.
const fn token(me: u8, client: &Client) -> u8 {
    me * 4 + client.attempts
}

/// Владелец токена.
const fn owner_of(token: u8) -> u8 {
    token / 4
}

impl State {
    fn initial() -> Self {
        let client = Client {
            pc: Pc::Start,
            attempts: ATTEMPTS,
            g0: 0,
            g: 0,
            leave_now: false,
        };
        Self {
            claim: 0,
            cs: Cs::Idle,
            generation: 0,
            server_ready: false,
            client_pid: 0,
            req: false,
            ack: false,
            disconnect: 0,
            clients: [client; CLIENTS],
            server: Server {
                pc: Spc::Idle,
                t: 0,
                pid: 0,
                t2: 0,
                pid2: 0,
                present: false,
                peer: 0,
                sclaim: 0,
                owner: 0,
                connected: false,
                kicked: false,
            },
            commits: [0; GENERATIONS],
            accepted: [0; GENERATIONS],
        }
    }

    /// Инвариант I2 (проверяется после каждого перехода, только ревизия 3).
    fn check_i2(&self) -> Result<(), String> {
        match self.cs {
            Cs::Hello(c) | Cs::Ready(c) if self.claim == 0 || owner_of(self.claim) != c => Err(
                format!("I2: client_state {:?} при claim {}", self.cs, self.claim),
            ),
            _ => Ok(()),
        }
    }
}

/// Следующая попытка клиента (или конец).
const fn next_attempt(client: &mut Client) {
    client.attempts -= 1;
    client.leave_now = false;
    client.pc = if client.attempts == 0 {
        Pc::Done
    } else {
        Pc::Start
    };
}

/// Все переходы клиента `i` из состояния `s`.
fn client_steps(rev: Revision, s: &State, i: usize, out: &mut Vec<Result<State, String>>) {
    let me = u8::try_from(i + 1).unwrap_or(u8::MAX);
    let c = s.clients[i];
    let tok = token(me, &c);
    let mut n = *s;
    let three = rev != Revision::Two;
    match c.pc {
        Pc::Done => {}
        Pc::Start => {
            // C1: CAS `slot_claim: 0 -> T` (ревизия 2 -- без claim).
            if !three || n.claim == 0 {
                if three {
                    n.claim = tok;
                }
                n.clients[i].pc = Pc::C3;
            } else {
                // Проигрыш: толчок, если сервер не в сессии.
                if !n.server_ready {
                    n.req = true;
                }
                next_attempt(&mut n.clients[i]);
            }
            out.push(Ok(n));
        }
        Pc::C3 => {
            n.clients[i].g0 = n.generation;
            n.clients[i].pc = Pc::C4;
            out.push(Ok(n));
        }
        Pc::C4 => {
            n.ack = false;
            n.clients[i].pc = Pc::C5;
            out.push(Ok(n));
        }
        Pc::C5 => {
            n.client_pid = me;
            n.clients[i].pc = Pc::C6;
            out.push(Ok(n));
        }
        Pc::C6 => {
            if three {
                // CAS `IDLE -> CLIENT_HELLO`.
                if n.cs == Cs::Idle {
                    n.cs = Cs::Hello(me);
                    n.clients[i].pc = Pc::C8;
                } else {
                    n.clients[i].pc = Pc::RollbackPid;
                }
            } else {
                // Ревизия 2: store поверх чего угодно.
                n.cs = Cs::Hello(me);
                n.clients[i].pc = Pc::C9;
            }
            out.push(Ok(n));
        }
        Pc::C8 => {
            let mine = n.claim == tok;
            n.clients[i].pc = if mine { Pc::C9 } else { Pc::Withdraw };
            n.clients[i].leave_now = !mine;
            out.push(Ok(n));
        }
        Pc::C9 => {
            n.req = true;
            n.clients[i].pc = Pc::Wait;
            out.push(Ok(n));
        }
        Pc::Wait => {
            // Сигнал `S2C_CONNECT` (потребляется ожиданием).
            if n.ack {
                let mut m = n;
                m.ack = false;
                // Отклонение от дизайна §2.1 C10 (найдено этой моделью):
                // сигнал при стоящей заявке (`CLIENT_HELLO`) -- всегда
                // устаревший, даже при `generation != g0`: это S7 прошлой
                // сессии, а сервер опубликовал S3 новой, но ещё не
                // зафиксировал её (S5).
                let accepted = if rev == Revision::DesignC10 {
                    matches!(m.cs, Cs::Ready(_)) || m.generation != c.g0
                } else if three {
                    match m.cs {
                        Cs::Ready(_) => true,
                        Cs::Idle => m.generation != c.g0,
                        Cs::Hello(_) => false,
                    }
                } else {
                    m.generation != c.g0
                };
                if accepted {
                    m.clients[i].pc = Pc::Accept;
                }
                out.push(Ok(m));
            }
            // Таймаут/остановка -- отзыв.
            n.clients[i].pc = Pc::Withdraw;
            out.push(Ok(n));
        }
        Pc::Withdraw => {
            // CAS `CLIENT_HELLO -> IDLE`.
            match n.cs {
                Cs::Hello(owner) => {
                    if three && owner != me {
                        out.push(Err(format!(
                            "I2: клиент {me} отзывает чужую заявку {owner}"
                        )));
                        return;
                    }
                    n.cs = Cs::Idle;
                    n.clients[i].pc = Pc::RollbackPid;
                }
                Cs::Ready(_) if three => n.clients[i].pc = Pc::Accept,
                _ if n.generation != c.g0 => n.clients[i].pc = Pc::Accept,
                _ => n.clients[i].pc = Pc::RollbackPid,
            }
            out.push(Ok(n));
        }
        Pc::RollbackPid => {
            n.client_pid = 0;
            if three {
                n.clients[i].pc = Pc::Release;
            } else {
                next_attempt(&mut n.clients[i]);
            }
            out.push(Ok(n));
        }
        Pc::Release | Pc::L4 => {
            if n.claim == tok {
                n.claim = 0;
            }
            next_attempt(&mut n.clients[i]);
            out.push(Ok(n));
        }
        Pc::Accept => {
            let g = n.generation;
            let gi = usize::from(g);
            // I4: сессия этого `generation` зафиксирована на нашей заявке и
            // никем больше не засчитана.
            if n.commits[gi] != me {
                out.push(Err(format!(
                    "I4: клиент {me} принят к сессии {g} владельца {}",
                    n.commits[gi]
                )));
                return;
            }
            if n.accepted[gi] != 0 {
                out.push(Err(format!(
                    "I4: сессию {g} засчитали клиенты {} и {me}",
                    n.accepted[gi]
                )));
                return;
            }
            n.accepted[gi] = me;
            n.clients[i].g = g;
            n.clients[i].pc = if c.leave_now { Pc::L1 } else { Pc::Conn };
            out.push(Ok(n));
        }
        Pc::Conn => {
            // Уход по желанию.
            let mut leave = n;
            leave.clients[i].pc = Pc::L1;
            out.push(Ok(leave));
            // Worker (Auto) разбирает `DISCONNECT`: любой сигнал -- конец
            // сессии; `mark_disconnected` клиента -- при уходе только CAS
            // `T -> 0`. Сигнал обязан быть от сервера этой сессии.
            if n.disconnect != 0 {
                if three && n.disconnect != SERVER {
                    out.push(Err(format!(
                        "G1: клиент {me} разобрал DISCONNECT клиента {}",
                        n.disconnect
                    )));
                    return;
                }
                n.disconnect = 0;
                n.clients[i].pc = Pc::L4;
                out.push(Ok(n));
            }
        }
        Pc::L1 => {
            n.clients[i].pc = if !three || n.generation == c.g {
                Pc::L2
            } else {
                Pc::L4
            };
            out.push(Ok(n));
        }
        Pc::L2 => {
            if let Cs::Ready(owner) = n.cs {
                if owner != me {
                    out.push(Err(format!(
                        "I5: уход клиента {me} снимает сессию клиента {owner}"
                    )));
                    return;
                }
                n.cs = Cs::Idle;
            }
            n.clients[i].pc = Pc::L3;
            out.push(Ok(n));
        }
        Pc::L3 => {
            if n.server.connected && n.server.owner != me {
                out.push(Err(format!(
                    "I5: DISCONNECT клиента {me} в сессии клиента {}",
                    n.server.owner
                )));
                return;
            }
            n.disconnect = me;
            if three {
                n.clients[i].pc = Pc::L4;
            } else {
                next_attempt(&mut n.clients[i]);
            }
            out.push(Ok(n));
        }
        Pc::Forgotten => {
            // Поздний уход «забытого» клиента -- одним переходом (A1).
            if n.generation == c.g {
                if let Cs::Ready(owner) = n.cs {
                    if owner != me {
                        out.push(Err(format!("I5: поздний уход {me} снимает сессию {owner}")));
                        return;
                    }
                    n.cs = Cs::Idle;
                }
                if n.server.connected && n.server.owner != me {
                    out.push(Err(format!(
                        "I5: поздний DISCONNECT {me} в сессии {}",
                        n.server.owner
                    )));
                    return;
                }
                n.disconnect = me;
            }
            if n.claim == tok {
                n.claim = 0;
            }
            next_attempt(&mut n.clients[i]);
            out.push(Ok(n));
        }
    }
}

/// `mark_disconnected` сервера: `IDLE`, CAS `SERVER_READY -> IDLE`.
const fn mark_disconnected(n: &mut State) {
    n.server.connected = false;
    n.server.peer = 0;
    n.server_ready = false;
    if let Cs::Ready(_) = n.cs {
        n.cs = Cs::Idle;
    }
}

/// Все переходы сервера.
#[expect(
    clippy::too_many_lines,
    reason = "один `match` по шагам сервера -- читается как таблица §2.2"
)]
fn server_steps(rev: Revision, s: &State, out: &mut Vec<Result<State, String>>) {
    let mut n = *s;
    let three = rev != Revision::Two;
    match s.server.pc {
        Spc::Idle => {
            if n.req {
                n.req = false;
                if matches!(n.cs, Cs::Hello(_)) {
                    n.server.pc = Spc::S1;
                    out.push(Ok(n));
                } else {
                    out.push(Ok(n));
                    // Толчок без заявки: отъём протухшего захвата (§2.3) --
                    // по A1 только у клиента, «не заметившего» отключения
                    // сервером (он в сессии, которой уже нет).
                    if three && n.claim != 0 {
                        let holder = usize::from(owner_of(n.claim) - 1);
                        let c = n.clients[holder];
                        if c.pc == Pc::Conn && n.generation == c.g && !n.server.connected {
                            let mut m = n;
                            m.claim = 0;
                            m.clients[holder].pc = Pc::Forgotten;
                            out.push(Ok(m));
                        }
                    }
                }
            }
        }
        Spc::S1 => {
            // I4: самоописание владельца заявки к S1 на месте.
            if let Cs::Hello(owner) = n.cs
                && three
                && n.client_pid != owner
            {
                out.push(Err(format!(
                    "I4: к S1 заявки клиента {owner} в client_pid {}",
                    n.client_pid
                )));
                return;
            }
            n.server.t = n.claim;
            n.server.pid = n.client_pid;
            n.server.pc = Spc::S3;
            out.push(Ok(n));
        }
        Spc::S3 => {
            n.generation += 1;
            n.server_ready = true;
            n.server.pc = Spc::S4;
            out.push(Ok(n));
        }
        Spc::S4 => {
            // Ревизия 3: сброс `DISCONNECT` до фиксации; ревизия 2 -- после.
            if three {
                n.disconnect = 0;
            }
            n.server.pc = Spc::S5;
            out.push(Ok(n));
        }
        Spc::S5 => {
            if let Cs::Hello(owner) = n.cs {
                n.cs = Cs::Ready(owner);
                n.server.owner = owner;
                n.commits[usize::from(n.generation)] = owner;
                n.server.pc = Spc::S6;
            } else {
                n.server_ready = false;
                n.server.pc = Spc::Idle;
            }
            out.push(Ok(n));
        }
        Spc::S6 if !three => {
            // Ревизия 2: `swap` и сброс `DISCONNECT` после фиксации.
            n.server.peer = n.server.pid;
            n.client_pid = 0;
            n.disconnect = 0;
            n.server.present = true;
            n.server.pc = Spc::S7;
            out.push(Ok(n));
        }
        Spc::S6 if rev == Revision::DesignS6 => {
            // Дизайн S6: `swap` самоописания и `T2 = slot_claim` без сверки.
            let pid2 = n.client_pid;
            n.client_pid = 0;
            n.server.peer = if pid2 == n.server.pid {
                n.server.pid
            } else {
                pid2
            };
            n.server.sclaim = n.claim;
            n.server.present = true;
            n.server.pc = Spc::S7;
            out.push(Ok(n));
        }
        Spc::S6 => {
            n.server.t2 = n.claim;
            n.server.pc = Spc::S6Pid;
            out.push(Ok(n));
        }
        Spc::S6Pid => {
            n.server.pid2 = n.client_pid;
            n.server.pc = Spc::S6Check;
            out.push(Ok(n));
        }
        Spc::S6Check => {
            // Отклонение от дизайна §2.2 S6 (найдено этой моделью): `swap`
            // без сверки забирал самоописание СЛЕДУЮЩЕГО клиента, если
            // владелец сессии (принятый по проигранному отзыву) уже ушёл.
            // Claim и самоописание -- владельца, только пока он в сессии.
            n.server.present = matches!(n.cs, Cs::Ready(_));
            if n.server.present {
                n.server.peer = if n.server.pid2 == n.server.pid {
                    n.server.pid
                } else {
                    n.server.pid2
                };
                n.server.sclaim = n.server.t2;
                n.server.pc = Spc::S6Consume;
            } else {
                // Владелец ушёл: наблюдение и claim -- из S1, поля не трогаем.
                n.server.peer = n.server.pid;
                n.server.sclaim = n.server.t;
                n.server.pc = Spc::S7;
            }
            out.push(Ok(n));
        }
        Spc::S6Consume => {
            if n.client_pid == n.server.pid2 {
                n.client_pid = 0;
                n.server.pc = Spc::S6Verify;
            } else {
                n.server.pc = Spc::S7;
            }
            out.push(Ok(n));
        }
        Spc::S6Verify => {
            // Claim сменился -- владелец ушёл и следующий клиент мог уже
            // записать то же значение (тот же процесс): вернуть его.
            if n.claim != n.server.t2 && n.client_pid == 0 {
                n.client_pid = n.server.pid2;
            }
            n.server.pc = Spc::S7;
            out.push(Ok(n));
        }
        Spc::S7 => {
            if three
                && n.server.present
                && (n.server.peer != n.server.owner || owner_of(n.server.sclaim) != n.server.owner)
            {
                out.push(Err(format!(
                    "I4: сервер наблюдает {} (claim {}) в сессии клиента {}",
                    n.server.peer, n.server.sclaim, n.server.owner
                )));
                return;
            }
            n.ack = true;
            n.server.connected = true;
            n.server.pc = Spc::Conn;
            out.push(Ok(n));
        }
        Spc::Conn => {
            // Разбор `DISCONNECT` (worker Auto): любой сигнал -- уход
            // клиента. Сигнал обязан быть от клиента этой сессии (иначе --
            // разрыв чужой сессии, G1/призрак).
            if n.disconnect != 0 {
                if three && n.disconnect != n.server.owner {
                    out.push(Err(format!(
                        "G1: сервер разобрал DISCONNECT {} в сессии клиента {}",
                        n.disconnect, n.server.owner
                    )));
                    return;
                }
                let mut m = n;
                m.disconnect = 0;
                mark_disconnected(&mut m);
                m.server.pc = Spc::Idle;
                out.push(Ok(m));
            }
            // Отключение по инициативе сервера (один раз за прогон).
            if !n.server.kicked {
                let mut m = n;
                m.server.kicked = true;
                mark_disconnected(&mut m);
                m.server.pc = Spc::KickSignal;
                out.push(Ok(m));
            }
        }
        Spc::KickSignal => {
            n.disconnect = SERVER;
            n.server.pc = Spc::Idle;
            out.push(Ok(n));
        }
    }
}

/// Краткая запись состояния для контрпримера.
fn brief(s: &State) -> String {
    let c = |c: &Client| format!("{:?}(g0={},g={})", c.pc, c.g0, c.g);
    format!(
        "claim={} cs={:?} gen={} ready={} pid={} req={} ack={} disc={} | {} | {} | srv {:?} \
         peer={} owner={} conn={}",
        s.claim,
        s.cs,
        s.generation,
        s.server_ready,
        s.client_pid,
        s.req,
        s.ack,
        s.disconnect,
        c(&s.clients[0]),
        c(&s.clients[1]),
        s.server.pc,
        s.server.peer,
        s.server.owner,
        s.server.connected,
    )
}

/// Перебор всех достижимых состояний (BFS -- кратчайший контрпример);
/// первое нарушение -- `Err` с трассой от начального состояния.
fn explore(rev: Revision) -> Result<Stats, String> {
    let initial = State::initial();
    // Состояние -> предшественник (трасса контрпримера).
    let mut parent: HashMap<State, State> = HashMap::new();
    parent.insert(initial, initial);
    let mut queue = VecDeque::from([initial]);
    let mut next = Vec::new();
    let mut accepts = false;
    let trace = |parent: &HashMap<State, State>, mut at: State, violation: String| {
        let mut path = vec![brief(&at)];
        while let Some(&prev) = parent.get(&at) {
            if prev == at {
                break;
            }
            path.push(brief(&prev));
            at = prev;
        }
        path.reverse();
        format!("{violation}\n  трасса:\n    {}", path.join("\n    "))
    };
    while let Some(s) = queue.pop_front() {
        next.clear();
        for i in 0..CLIENTS {
            client_steps(rev, &s, i, &mut next);
        }
        server_steps(rev, &s, &mut next);
        for step in next.drain(..) {
            let n = match step {
                Ok(n) => n,
                Err(violation) => return Err(trace(&parent, s, violation)),
            };
            if rev != Revision::Two
                && let Err(violation) = n.check_i2()
            {
                return Err(trace(&parent, s, violation));
            }
            accepts |= n.accepted.iter().any(|&c| c != 0);
            if let std::collections::hash_map::Entry::Vacant(entry) = parent.entry(n) {
                entry.insert(s);
                queue.push_back(n);
            }
        }
    }
    Ok(Stats {
        states: parent.len(),
        accepts,
    })
}

#[test]
fn handshake_model() {
    let started = std::time::Instant::now();
    let stats = explore(Revision::Three).unwrap_or_else(|violation| panic!("{violation}"));
    println!(
        "handshake_model: ревизия 3 -- {} состояний, {:?}",
        stats.states,
        started.elapsed()
    );
    assert!(stats.accepts, "модель ни разу не подключила клиента");
    assert!(stats.states > 1_000, "подозрительно мало состояний");
}

/// Зубы модели: протокол ревизии 2 нарушает I4/I5 (Concern 1).
#[test]
fn handshake_model_finds_revision_two_defect() {
    let violation = explore(Revision::Two).expect_err("ревизия 2 обязана нарушать инварианты");
    println!("handshake_model: ревизия 2 -- {violation}");
    assert!(
        violation.starts_with("I4") || violation.starts_with("I5"),
        "{violation}"
    );
}

/// Отклонение §2.1 C10 нужно: без него стоящая заявка принимается по
/// устаревшему `S2C_CONNECT` прошлой сессии до фиксации (I4).
#[test]
fn handshake_model_design_c10_accepts_before_commit() {
    let violation = explore(Revision::DesignC10).expect_err("C10 дизайна обязан нарушать I4");
    println!("handshake_model: C10 дизайна -- {violation}");
    assert!(violation.starts_with("I4: клиент"), "{violation}");
}

/// Отклонение §2.2 S6 нужно: без сверки `swap` забирает самоописание (и
/// claim) следующего клиента -- сервер наблюдает не того (I4).
#[test]
fn handshake_model_design_s6_loses_next_description() {
    let violation = explore(Revision::DesignS6).expect_err("S6 дизайна обязан нарушать I4");
    println!("handshake_model: S6 дизайна -- {violation}");
    assert!(
        violation.starts_with("I4: к S1") || violation.starts_with("I4: сервер наблюдает"),
        "{violation}"
    );
}
