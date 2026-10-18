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
//! до `Opts::attempts` попыток подключения; на любом шаге ожидания возможен
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
//! у клиента, который «не заметил» отключения сервером (он «забыт»). С
//! решения AB его поздний уход -- обычный многошаговый L (сверка
//! `generation`, CAS пары `(client_state, slot_claim)`, `DISCONNECT`,
//! освобождение claim) с любыми чередованиями, в том числе отъёмом claim-а и
//! рукопожатием следующего клиента между сверкой и CAS: CAS пары не трогает
//! сессию с чужим claim-ом (с 32-битным CAS одного `client_state` перебор
//! находит нарушение I5). Остаётся допущение A1' (N2 повторного ревью
//! T11b): отъём моделируется только у клиента в `Conn`, поэтому окно МЕЖДУ
//! удачным CAS пары (L2) и сигналом `DISCONNECT` (L3) не исследуется. Чтобы
//! поздний сигнал попал в сессию следующего клиента, поток уходящего должен
//! быть вытеснен ровно там дольше, чем сервер закроет сессию, отнимет claim
//! (он ещё наш до L4) по `RESERVE_TIMEOUT` от первого толчка и примет
//! следующего; исход -- не заклинивание, а лишний `DISCONNECT` в новой
//! сессии, который любой потребитель Rust (синхронные ожидания, async,
//! worker-ы Auto, диспетчер Multi и `MultiClient`; T11c) примет за
//! устаревший: клиент B в сессии, `client_state == SERVER_READY` -- сигнал
//! поглощается, сессия B живёт. В варианте `Revision::Two`
//! поздний уход -- по-прежнему один переход. Смерти
//! процессов (S2, освобождение claim умершего) -- в unit-тестах
//! `server.rs`, не в модели. PID клиента в модели -- его номер (разные
//! клиенты -- разные процессы; совпадение PID одного процесса моделирует
//! повторная попытка того же клиента).
//!
//! **Граница перебора** (round 3, решение AI). Два клиента, до
//! `Opts::attempts` попыток у каждого (по умолчанию в наборе тестов -- 2;
//! `#[ignore]`-прогон -- 3: около 73 млн состояний, ~1,5 мин в release),
//! сервер отключает клиента сам не больше одного раза. Свойства доказаны
//! для этой границы; при 2 попытках перебор пропустил нарушение I4 после
//! отъёма живого держателя (повторное ревью 2), найденное при 3, -- поэтому
//! 3-попыточный прогон обязателен при правке протокола. 4 попытки в разумные
//! время и память не помещаются (оценка -- на порядок больше состояний).
//!
//! **Отъём R-O живого держателя исключён допущением A1'** (round 3,
//! решение AI). Быстрый путь сервера снимает claim, чей владелец
//! (`slot_owner_pid` + `owner_start`) подтверждённо умер, но только тройку
//! (токен, PID, время), увиденную умершей не меньше `OWNER_DEATH_CONFIRM`
//! назад и та же сейчас. Поля владельца держатель пишет на C2 -- двумя
//! записями сразу после C1; живой держатель с полями прежнего (умершего, не
//! очистившего) владельца был бы снят, только если его поток простоял
//! между двумя соседними записями дольше `OWNER_DEATH_CONFIRM` (A1' -- того
//! же рода, что A1). Поэтому в модели живых держателей не грабят. Вариант
//! `Opts::live_rob` (отъём без подтверждения: решение по полям в окне C1-C2,
//! снятие -- отдельными шагами, держатель тем временем проходит C2..C6;
//! начальное поле владельца -- `DEAD`) перебор обязан опровергнуть: живой
//! ограбленный остаётся в сессии-призраке (свойство **W**: все клиенты
//! ушли, а `client_state != IDLE`), а при 3 попытках -- и нарушением I4
//! (отзыв ограбленного снимает заявку следующего, тот засчитывает
//! устаревший `S2C_CONNECT`). Компенсации fix round 1 и follow-up (возврат
//! claim-а на C8, запасной уход по `generation`) и защита фиксации по
//! самоописанию (вариант (a) решения AI: в окне C1-C2 следующего держателя
//! поля владельца -- ещё ограбленного, и защита пропускает его заявку)
//! этого не закрывают -- проверено перебором при 3 попытках; они удалены.
//! После отъёма I2 намеренно нарушен, поэтому в варианте `live_rob` I2 и
//! проверки самоописания S1/S7 сняты.
//!
//! **Чего модель НЕ моделирует** (fix round 1, находка 3 ревью):
//! - отъём протухшего захвата по `RESERVE_TIMEOUT` у держателя между C1 и
//!   C6 (нарушение A1 -- вне модели, §h п. 10 INTEROP); с Task 12 ветку C8
//!   открывает отъём R-O (выше);
//! - режимы `ClaimMode::Held` (Multi: claim берёт перебор) и
//!   `TakeOverAbandoned` (лобби с брошенным мьютексом), `abort_session` S6 и
//!   смерти процессов вообще -- S2, освобождение claim умершего в
//!   `mark_disconnected`, перехват claim-а умершего. Всё это требует смертей
//!   (процесс, переставший делать шаги, плюс наблюдение `is_process_gone`),
//!   а с ними пространство состояний растёт кратно; поэтому перехват только
//!   из `IDLE` и перепроверка claim в S2 (fix round 1) в модель не добавлены
//!   -- их закрепляют детерминированные тесты
//!   `client::tests::takeover_only_from_idle` и
//!   `server::handshake_tests::dead_hello_withdrawal_rechecks_claim`.
//!
//! **Слитые чтения.** Несколько шагов читают два слова одним переходом:
//! C10 и W -- `client_state`, затем `generation`; S1 -- `slot_claim` и
//! `client_pid`. Это корректно, пока клиент держит claim: `generation`
//! меняет только S3, а S3 бывает только на `CLIENT_HELLO`, который ставит
//! только держатель claim-а (I2). Значит, между двумя чтениями C10/W
//! (заявка уже не стоит: она зафиксирована или отозвана) `generation` не
//! меняется; а в S1 при стоящей заявке ни claim, ни `client_pid` никто,
//! кроме держателя, не пишет (I3), а держатель меняет их только после
//! своего отзыва W (CAS уже снял заявку) -- тогда S5 провалится, и
//! прочитанное в S1 в модели не используется. Оговорка: немоделируемый S2
//! чтения S1 использует -- и именно там они могут устареть (прежний клиент
//! отозвал заявку, освободил claim и умер, а живой следующий уже поставил
//! свою: находка 2 ревью, fix round 1). Это закрывает перепроверка
//! `slot_claim == T` непосредственно перед CAS S2
//! (`server::handshake_tests::dead_hello_withdrawal_rechecks_claim`). S6,
//! где владелец мог уже уйти, напротив, разбит на отдельные чтения --
//! именно там модель нашла пробел дизайна.
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

/// Клиенты: 1 и 2 (`0` -- «никто»: свободный claim, нулевой PID).
const CLIENTS: usize = 2;
/// Метка «`DISCONNECT` взвёл сервер».
const SERVER: u8 = 3;
/// Предел `generation` в модели (рукопожатий за прогон не больше
/// `CLIENTS × attempts`, то есть до 3 попыток). Не больше нужного: массивы
/// призраков -- в каждом состоянии, а 3-попыточный прогон и так занимает
/// несколько ГБ.
const GENERATIONS: usize = 8;
/// Шаг токена на клиента (токен = `клиент × TOKEN_STRIDE + попытка`).
const TOKEN_STRIDE: u8 = 8;

/// Параметры перебора (round 3, решение AI).
#[derive(Clone, Copy, Debug)]
struct Opts {
    /// Попыток подключения на клиента.
    attempts: u8,
    /// Отъём R-O живого держателя (без подтверждения смерти во времени):
    /// вариант, который перебор обязан опровергнуть.
    live_rob: bool,
    /// Follow-up N2: C1 удался, а `client_state != IDLE` -- сразу
    /// освободить claim (без C2/C5) и толкнуть сервер.
    early_c1: bool,
    /// Проверять свойство W (без сессии-призрака). Снято -- чтобы увидеть,
    /// что ещё ломает отъём живого держателя (I4).
    check_w: bool,
}

impl Opts {
    /// Реализованный протокол.
    const fn current(attempts: u8) -> Self {
        Self {
            attempts,
            live_rob: false,
            early_c1: true,
            check_w: true,
        }
    }

    /// Протокол с отъёмом живого держателя.
    const fn live_rob(attempts: u8) -> Self {
        Self {
            live_rob: true,
            ..Self::current(attempts)
        }
    }
}

/// Метка поля владельца claim-а: личность прежнего держателя, уже
/// умершего и не очистившего её (пир, не выполняющий R18 «очистить поля
/// владельца перед освобождением»).
const DEAD: u8 = 9;

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
    /// C2: поля владельца claim-а -- свои (fix round 1: отдельный шаг --
    /// окно C1-C2, где поля ещё прежнего держателя).
    C2,
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
    /// Быстрый путь R-O (Task 12): решение принято по полям владельца
    /// (`t` -- claim); очистить поле владельца (CAS `DEAD -> 0`; изменилось
    /// -- отказ).
    RobForget,
    /// R-O: CAS `slot_claim: t -> 0` (fix round 1 (b): только при `IDLE`).
    RobCas,
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
    /// Поле владельца claim-а (`slot_owner_pid` + `owner_start`): `0` --
    /// очищено, клиент `1`/`2`, `DEAD` -- прежний держатель-пир, умерший, не
    /// очистив его (начальное состояние модели).
    owner: u8,
    /// Призрак: claim текущей попытки клиента отнят R-O -- её заявка или
    /// сессия может стоять при чужом/свободном claim-е (I2 нарушен).
    robbed: [bool; CLIENTS],
    /// Призрак: отъём живого держателя случился в этом прогоне.
    any_rob: bool,
}

/// Итог перебора.
#[derive(Debug)]
struct Stats {
    states: usize,
    accepts: bool,
    /// Достигнут отъём R-O живого держателя (fix round 1).
    robs: bool,
}

/// Токен попытки: уникален на (клиент, попытку), как у `claim_token`.
const fn token(me: u8, client: &Client) -> u8 {
    me * TOKEN_STRIDE + client.attempts
}

/// Владелец токена.
const fn owner_of(token: u8) -> u8 {
    token / TOKEN_STRIDE
}

impl State {
    fn initial(o: &Opts) -> Self {
        let client = Client {
            pc: Pc::Start,
            attempts: o.attempts,
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
            owner: DEAD,
            robbed: [false; CLIENTS],
            any_rob: false,
        }
    }

    /// Инвариант I2 (проверяется после каждого перехода, только ревизия 3)
    /// -- пока в прогоне не было отъёма R-O живого держателя: отъём нарушает
    /// I2 намеренно (заявка или сессия ограбленного стоит при чужом или
    /// свободном claim-е, и пока она стоит, другой держатель может и
    /// выставить, и отозвать заявку поверх неё). После отъёма охраняют I4,
    /// I5, G1 и W.
    fn check_i2(&self) -> Result<(), String> {
        match self.cs {
            Cs::Hello(c) | Cs::Ready(c)
                if (self.claim == 0 || owner_of(self.claim) != c) && !self.any_rob =>
            {
                Err(format!(
                    "I2: client_state {:?} при claim {}",
                    self.cs, self.claim
                ))
            }
            _ => Ok(()),
        }
    }

    /// Свойство W (fix round 1, I1): все клиенты закончили -- ни заявки,
    /// ни сессии в канале (сессия-призрак заклинила бы сервер до выхода
    /// процесса ушедшего клиента: каждый следующий -- `AlreadyConnected`).
    fn check_no_ghost(&self) -> Result<(), String> {
        let done = self.clients.iter().all(|c| c.pc == Pc::Done);
        if done && self.cs != Cs::Idle {
            return Err(format!(
                "W: все клиенты ушли, а client_state {:?} (сессия-призрак)",
                self.cs
            ));
        }
        Ok(())
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

/// Следующая попытка клиента `i`: призрак «claim отнят» -- только у этой
/// попытки.
const fn next_attempt_of(n: &mut State, i: usize) {
    n.robbed[i] = false;
    next_attempt(&mut n.clients[i]);
}

/// Освобождение claim-а клиентом (откат, отзыв, уход): fix round 1 --
/// сначала очистить свои поля владельца (R18), затем CAS `T -> 0`.
const fn release_own(n: &mut State, me: u8, tok: u8) {
    if n.claim == tok {
        if n.owner == me {
            n.owner = 0;
        }
        n.claim = 0;
    }
}

/// Все переходы клиента `i` из состояния `s`.
#[expect(
    clippy::too_many_lines,
    reason = "один `match` по шагам клиента -- читается как таблица §2.1"
)]
fn client_steps(
    rev: Revision,
    o: &Opts,
    s: &State,
    i: usize,
    out: &mut Vec<Result<State, String>>,
) {
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
                n.clients[i].pc = if three { Pc::C2 } else { Pc::C3 };
                // Follow-up N2 (слитое чтение `client_state` за C1): заявка
                // или сессия уже стоит -- сразу освободить claim (поля
                // владельца не трогались) и толкнуть сервер.
                if three && o.early_c1 && n.cs != Cs::Idle {
                    n.claim = 0;
                    if matches!(n.cs, Cs::Hello(_)) && !n.server_ready {
                        n.req = true;
                    }
                    next_attempt_of(&mut n, i);
                }
            } else {
                // Проигрыш: толчок, если сервер не в сессии.
                if !n.server_ready {
                    n.req = true;
                }
                next_attempt_of(&mut n, i);
            }
            out.push(Ok(n));
        }
        Pc::C2 => {
            n.owner = me;
            n.clients[i].pc = Pc::C3;
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
                    if three && owner != me && !n.any_rob {
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
            // Follow-up N3: своё самоописание -- CAS от своего значения.
            if !three || n.client_pid == me {
                n.client_pid = 0;
            }
            if three {
                n.clients[i].pc = Pc::Release;
            } else {
                next_attempt_of(&mut n, i);
            }
            out.push(Ok(n));
        }
        Pc::Release | Pc::L4 => {
            release_own(&mut n, me, tok);
            next_attempt_of(&mut n, i);
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
            // Ревизия 3 (решение AB): CAS ПАРЫ `(client_state, slot_claim)`:
            // `(SERVER_READY, T) -> (IDLE, T)`; не прошёл -- ни `IDLE`, ни
            // `DISCONNECT` (T11b), сразу освобождение claim. Ревизия 2 --
            // прежний CAS одного `client_state` и сигнал всегда.
            let ready = matches!(n.cs, Cs::Ready(_));
            if three && !(ready && n.claim == tok) {
                n.clients[i].pc = Pc::L4;
                out.push(Ok(n));
                return;
            }
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
                next_attempt_of(&mut n, i);
            }
            out.push(Ok(n));
        }
        Pc::Forgotten if three => {
            // Решение AB: поздний уход «забытого» клиента -- обычный
            // многошаговый L (L1 сверка `generation`, L2 CAS пары, L3, L4) с
            // любыми чередованиями: в том числе отъём claim-а и рукопожатие
            // следующего клиента между L1 и L2. Допущение A1 для ухода больше
            // не нужно -- CAS пары не трогает сессию с чужим claim-ом.
            n.clients[i].pc = Pc::L1;
            out.push(Ok(n));
        }
        Pc::Forgotten => {
            // Ревизия 2: поздний уход «забытого» клиента -- одним переходом
            // (A1).
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
            release_own(&mut n, me, tok);
            next_attempt_of(&mut n, i);
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
fn server_steps(rev: Revision, o: &Opts, s: &State, out: &mut Vec<Result<State, String>>) {
    let mut n = *s;
    let three = rev != Revision::Two;
    match s.server.pc {
        Spc::Idle => {
            if n.req {
                n.req = false;
                // Быстрый путь R-O на толчке (Task 12): поля владельца --
                // личность умершего прежнего держателя -- решение «снять
                // claim `t`». Само снятие -- шагами `RobForget`/`RobCas`
                // (держатель тем временем может пройти C2..C6).
                if three && o.live_rob && n.claim != 0 && n.owner == DEAD {
                    let mut m = n;
                    m.server.t = n.claim;
                    m.server.pc = Spc::RobForget;
                    out.push(Ok(m));
                }
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
            // I4: самоописание владельца заявки к S1 на месте. После отъёма
            // живого держателя (только с пиром, не очищающим поля владельца
            // -- начальное `DEAD`) ограбленный пишет `client_pid` (C5) и
            // обнуляет его (откат) поверх чужой заявки: I3 не держится, и
            // самоописание может быть затёрто -- остаток, вне I4 (§h п. 12).
            if let Cs::Hello(owner) = n.cs
                && three
                && n.client_pid != owner
                && !n.any_rob
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
                && !n.any_rob
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
        Spc::RobForget => {
            // Fix round 1 (b): очистить поле умершего владельца CAS-ом;
            // изменилось (держатель прошёл C2) -- отказ.
            if n.owner == DEAD {
                n.owner = 0;
                n.server.pc = Spc::RobCas;
            } else {
                n.server.pc = Spc::Idle;
            }
            out.push(Ok(n));
        }
        Spc::RobCas => {
            // `client_state` перечитан прямо перед CAS -- только `IDLE`.
            if n.cs == Cs::Idle && n.claim == n.server.t {
                let holder = usize::from(owner_of(n.claim) - 1);
                n.claim = 0;
                n.robbed[holder] = true;
                n.any_rob = true;
            }
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

/// Отпечаток состояния (сжатие хешем, как у TLC/SPIN): 64 бита SipHash с
/// постоянными ключами. Множество посещённых хранит только отпечатки --
/// 3-попыточный прогон (~73 млн состояний) иначе не помещается в память.
/// Коллизия может лишь СКРЫТЬ состояние (не дать ложного нарушения):
/// вероятность хотя бы одной ~ n^2 / 2^65 -- около 1e-7 при 2 попытках и
/// около 1e-4 при 3.
fn fingerprint(s: &State) -> u64 {
    use std::hash::{Hash, Hasher};
    #[expect(
        deprecated,
        reason = "SipHasher -- единственный в std хешер с постоянными ключами"
    )]
    let mut hasher = std::hash::SipHasher::new();
    s.hash(&mut hasher);
    hasher.finish()
}

/// Хешер таблицы отпечатков: отпечаток уже равномерен -- как есть.
#[derive(Default)]
struct Identity(u64);

impl std::hash::Hasher for Identity {
    fn finish(&self) -> u64 {
        self.0
    }

    fn write(&mut self, bytes: &[u8]) {
        for &byte in bytes {
            self.0 = self.0.rotate_left(8) ^ u64::from(byte);
        }
    }

    fn write_u64(&mut self, value: u64) {
        self.0 = value;
    }
}

type Visited = HashMap<u64, u64, std::hash::BuildHasherDefault<Identity>>;

/// Все переходы из `s` (ошибка шага -- нарушение на самом переходе).
fn successors(rev: Revision, o: &Opts, s: &State, next: &mut Vec<Result<State, String>>) {
    next.clear();
    for i in 0..CLIENTS {
        client_steps(rev, o, s, i, next);
    }
    server_steps(rev, o, s, next);
}

/// m-c: трасса от начального состояния до `at` (отпечатки -- в `parent`)
/// и, если задано, следующее состояние `last` -- нарушение свойства самого
/// нового. Состояния восстанавливаются повторным проходом вперёд по цепочке
/// отпечатков.
fn trace(
    rev: Revision,
    o: &Opts,
    parent: &Visited,
    at: &State,
    last: Option<&State>,
    violation: &str,
) -> String {
    let mut chain = vec![fingerprint(at)];
    while let Some(&prev) = chain.last().and_then(|fp| parent.get(fp)) {
        if Some(&prev) == chain.last() {
            break;
        }
        chain.push(prev);
    }
    chain.reverse();
    let mut state = State::initial(o);
    let mut path = vec![brief(&state)];
    let mut next = Vec::new();
    for &fp in chain.iter().skip(1) {
        successors(rev, o, &state, &mut next);
        let Some(found) = next
            .iter()
            .filter_map(|step| step.as_ref().ok())
            .find(|n| fingerprint(n) == fp)
        else {
            path.push("(трасса прервана: коллизия отпечатков)".to_owned());
            break;
        };
        state = *found;
        path.push(brief(&state));
    }
    path.extend(last.map(brief));
    format!("{violation}\n  трасса:\n    {}", path.join("\n    "))
}

/// Перебор всех достижимых состояний (BFS -- кратчайший контрпример);
/// первое нарушение -- `Err` с трассой от начального состояния.
fn explore(rev: Revision, o: &Opts) -> Result<Stats, String> {
    assert!(
        usize::from(o.attempts) * CLIENTS < GENERATIONS,
        "граница попыток больше предела `generation`"
    );
    let initial = State::initial(o);
    // Отпечаток состояния -> отпечаток предшественника (трасса).
    let mut parent = Visited::default();
    parent.insert(fingerprint(&initial), fingerprint(&initial));
    let mut queue = VecDeque::from([initial]);
    let mut next = Vec::new();
    let mut accepts = false;
    let mut robs = false;
    while let Some(s) = queue.pop_front() {
        successors(rev, o, &s, &mut next);
        let from = fingerprint(&s);
        for step in next.drain(..) {
            let n = match step {
                Ok(n) => n,
                Err(violation) => return Err(trace(rev, o, &parent, &s, None, &violation)),
            };
            if rev != Revision::Two
                && let Err(violation) = n.check_i2().and_then(|()| {
                    if o.check_w {
                        n.check_no_ghost()
                    } else {
                        Ok(())
                    }
                })
            {
                return Err(trace(rev, o, &parent, &s, Some(&n), &violation));
            }
            accepts |= n.accepted.iter().any(|&c| c != 0);
            robs |= n.any_rob;
            if let std::collections::hash_map::Entry::Vacant(entry) = parent.entry(fingerprint(&n))
            {
                entry.insert(from);
                queue.push_back(n);
            }
        }
    }
    Ok(Stats {
        states: parent.len(),
        accepts,
        robs,
    })
}

/// Прогон модели реализованного протокола с границей `attempts`.
fn check_current(attempts: u8) -> Stats {
    let started = std::time::Instant::now();
    let stats = explore(Revision::Three, &Opts::current(attempts))
        .unwrap_or_else(|violation| panic!("{violation}"));
    println!(
        "handshake_model: ревизия 3, {attempts} попытки -- {} состояний, {:?}",
        stats.states,
        started.elapsed()
    );
    assert!(stats.accepts, "модель ни разу не подключила клиента");
    assert!(stats.states > 1_000, "подозрительно мало состояний");
    assert!(!stats.robs, "живой держатель ограблен");
    stats
}

#[test]
fn handshake_model() {
    check_current(2);
}

/// Round 3 (решение AI): граница 3 попытки -- около 73 млн состояний,
/// ~1,5 мин и несколько ГБ памяти в release (`cargo test --release --lib --
/// --ignored handshake_model_three_attempts`). Обязателен при правке
/// протокола: граница 2 пропустила нарушение I4 (повторное ревью 2).
#[test]
#[ignore = "долгий: ~1,5 мин в release; при правке протокола -- обязателен"]
fn handshake_model_three_attempts() {
    check_current(3);
}

/// Round 3 (решение AI): отъём R-O живого держателя (без подтверждения
/// смерти во времени) -- сессия-призрак (W) уже при 2 попытках; поэтому
/// быстрый путь снимает только тройку, умершую не меньше
/// `OWNER_DEATH_CONFIRM` назад (A1').
#[test]
fn handshake_model_live_rob_leaves_ghost() {
    let violation = explore(Revision::Three, &Opts::live_rob(2))
        .expect_err("отъём живого держателя обязан нарушать свойства");
    println!("handshake_model: отъём живого держателя -- {violation}");
    assert!(violation.starts_with("W:"), "{violation}");
}

/// Round 3 (повторное ревью 2, I1): при 3 попытках отъём живого держателя
/// нарушает и I4 (контрпример повторного ревью 2: «клиент 2 принят к сессии
/// 1 владельца 1») -- отзыв ограбленного снимает заявку следующего, тот
/// засчитывает устаревший `S2C_CONNECT`. W снято, иначе BFS раньше находит
/// сессию-призрак; контрпример короткий, прогон дешёвый.
#[test]
fn handshake_model_live_rob_breaks_i4_at_three_attempts() {
    let o = Opts {
        check_w: false,
        ..Opts::live_rob(3)
    };
    let violation =
        explore(Revision::Three, &o).expect_err("отъём живого держателя обязан нарушать I4");
    println!("handshake_model: отъём живого держателя, 3 попытки -- {violation}");
    assert!(violation.starts_with("I4"), "{violation}");
}

/// Зубы модели: протокол ревизии 2 нарушает I4/I5 (Concern 1).
#[test]
fn handshake_model_finds_revision_two_defect() {
    let violation = explore(Revision::Two, &Opts::current(2))
        .expect_err("ревизия 2 обязана нарушать инварианты");
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
    let violation = explore(Revision::DesignC10, &Opts::current(2))
        .expect_err("C10 дизайна обязан нарушать I4");
    println!("handshake_model: C10 дизайна -- {violation}");
    assert!(violation.starts_with("I4: клиент"), "{violation}");
}

/// Отклонение §2.2 S6 нужно: без сверки `swap` забирает самоописание (и
/// claim) следующего клиента -- сервер наблюдает не того (I4).
#[test]
fn handshake_model_design_s6_loses_next_description() {
    let violation =
        explore(Revision::DesignS6, &Opts::current(2)).expect_err("S6 дизайна обязан нарушать I4");
    println!("handshake_model: S6 дизайна -- {violation}");
    assert!(
        violation.starts_with("I4: к S1") || violation.starts_with("I4: сервер наблюдает"),
        "{violation}"
    );
}
