//! Политика набора дыр для динамического режима (`PLAN-dynamic-holes-relay.md`, M4/M5): чистая
//! функция `plan()` решает, какие дыры открыть и какие слить, по снимку текущего набора. Никакой
//! сети, часов реального времени или RNG внутри — всё нужное передаётся параметрами, поэтому
//! поведение проверяется юнит-тестами без стенда. Сеть и часы — дело вызывающего (будущий
//! `HoleManager`, M5), который уже назначает дырам `id`/сессии и исполняет действия `plan()`.
//!
//! Случайный `max_age` на дыру (чтобы ТСПУ не видел ровного ритма переоткрытия) тоже не здесь:
//! `plan()` принимает уже назначенный `max_age` в каждой `HoleInfo` — генерирует его вызывающий
//! при открытии дыры (тем же `XorShift32`, что и `multilink::SlotPicker`), не на каждый вызов
//! `plan()` заново (иначе решение «просрочена» дёргалось бы между вызовами).

use std::time::Duration;

use crate::multilink::SlotId;

/// Идентификатор дыры в динамическом наборе — тот же тип, что и номер слота (`SlotId`): дыра
/// «поработала — умерла», номер не переиспользуется (`PLAN-dynamic-holes-relay.md`, раздел 1).
pub type HoleId = SlotId;

/// Фаза дыры в динамическом наборе (ещё не заведена в `multilink::LinkRegistry` — заводится в M5).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HoleState {
    /// Пробивается/ждёт первых пакетов; для данных ещё не выбирается.
    Warming,
    /// Прогрелась, в работе.
    Active,
    /// Обе стороны сливают: для данных уже не выбирается, но ещё не закрыта.
    Draining,
}

/// Снимок одной дыры на момент оценки политики. Потери — `None`, если выборка мала (тот же
/// принцип, что `multilink::loss`: на малой выборке доля ничего не значит).
#[derive(Clone, Copy, Debug)]
pub struct HoleInfo {
    pub id: HoleId,
    pub state: HoleState,
    /// Сколько дыра уже живёт.
    pub age: Duration,
    /// Срок жизни, назначенный этой дыре при открытии (случайно, в пределах `PoolPolicy::max_age`
    /// у вызывающего) — «просрочена», когда `age >= max_age`.
    pub max_age: Duration,
    /// Доля потерь от нас к пиру (мы посчитали `sent`, пир отчитался `received`).
    pub loss_out: Option<f32>,
    /// Доля потерь от пира к нам.
    pub loss_in: Option<f32>,
}

/// Что делать с набором дыр по итогам одной оценки.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Action {
    /// Открыть одну новую дыру (`id`/сессию назначит вызывающий — политика их не знает заранее).
    Open,
    /// Слить эту дыру (просьба `Drain`, не аварийная потеря).
    Retire(HoleId),
}

/// Настройки политики (владелец, `PLAN-dynamic-holes-relay.md`, раздел «Решения владельца»).
#[derive(Clone, Copy, Debug)]
pub struct PoolPolicy {
    /// Не меньше стольких живых (`Warming` + `Active`) дыр — ниже не опускаемся никогда.
    pub min_active: usize,
    /// Не больше стольких дыр всего (`Warming` + `Active` + `Draining`).
    pub max_total: usize,
    /// Не чаще, чем раз в столько, открываем дыру сверх `min_active` (пока не дойдём до `max_total`).
    pub add_interval: Duration,
    /// Доля потерь, после которой дыра считается однозначно плохой и сливается немедленно,
    /// независимо от сравнения с другими (тот же порог, что `multilink::link_is_bad`: доставлено
    /// меньше половины).
    pub bad_loss: f32,
    /// Порог плановой чистки: когда дыр больше минимума, худшая по потерям уходит, только если её
    /// потери выше этого (без порога любая случайная потеря одного пакета гоняла бы набор по кругу
    /// и держала его у минимума).
    pub cull_loss: f32,
    /// Новую дыру не сравниваем по потерям первые `grace` — ещё не накопила выборку/не остыла от
    /// всплеска на старте.
    pub grace: Duration,
}

impl Default for PoolPolicy {
    fn default() -> Self {
        Self {
            min_active: 4,
            max_total: 10,
            add_interval: Duration::from_secs(10),
            bad_loss: 0.5,
            cull_loss: 0.03,
            grace: Duration::from_secs(10),
        }
    }
}

fn is_live(h: &HoleInfo) -> bool {
    h.state != HoleState::Draining
}

/// Однозначно плохая прямо сейчас: за окном `grace` не льгота, доля потерь известна и превышает
/// `bad_loss` хоть в одну сторону.
fn is_badly_lossy(h: &HoleInfo, policy: &PoolPolicy) -> bool {
    if h.age < policy.grace {
        return false;
    }
    let bad = |loss: Option<f32>| loss.is_some_and(|l| l > policy.bad_loss);
    bad(h.loss_out) || bad(h.loss_in)
}

/// Потери дыры для сравнения «кто хуже»: худшая из двух сторон; `None` (мало данных) считается
/// «не хуже никого» — новые/нешумящие дыры не выталкивают прогретые по умолчанию.
fn worst_loss(h: &HoleInfo) -> f32 {
    h.loss_out.unwrap_or(0.0).max(h.loss_in.unwrap_or(0.0))
}

/// Снимок набора для `plan()`: состояние и потери берутся из реестра дыр, возраст и срок жизни — из
/// записей менеджера (`id`, когда открыл, назначенный срок). Дыры, которых ещё нет в реестре, —
/// `Warming` (идёт знакомство и пробив). Общий для менеджеров VPS-клиента и P2P.
pub(crate) fn snapshot(
    registry: &crate::multilink::LinkRegistry,
    records: impl Iterator<Item = (HoleId, std::time::Instant, Duration)>,
    now: std::time::Instant,
) -> Vec<HoleInfo> {
    records
        .map(|(id, opened, max_age)| {
            let age = now.duration_since(opened);
            match registry.hole_info(id) {
                Some((state, loss_out, loss_in)) => HoleInfo { id, state, age, max_age, loss_out, loss_in },
                None => HoleInfo { id, state: HoleState::Warming, age, max_age, loss_out: None, loss_in: None },
            }
        })
        .collect()
}

/// Решает, что делать с набором `holes` прямо сейчас. `since_last_open` — сколько прошло с
/// последнего `Action::Open` (вызывающий отслеживает это сам: политика не хранит состояние между
/// вызовами). Порядок правил — приоритет: более ранние строго важнее последующих, дальнейшие не
/// проверяются, если текущее правило уже дало действия (кроме правила 2 — рост, оно не мешает
/// проверить остальное в тот же момент оценки... на деле правила 1 и «однозначно плохая» —
/// срочные и забирают цикл целиком, рост и чистка — нет, см. тела веток).
pub fn plan(holes: &[HoleInfo], since_last_open: Duration, policy: &PoolPolicy) -> Vec<Action> {
    let live_count = holes.iter().filter(|h| is_live(h)).count();

    // 1. Критично мало (даже с учётом прогреваемых) — открыть недостающие сразу, ничего не сливать
    // в этом же цикле (сначала долечиться до минимума).
    if live_count < policy.min_active {
        return vec![Action::Open; policy.min_active - live_count];
    }

    // 5. Однозначно плохая дыра — сливаем немедленно и тут же открываем замену, независимо от
    // прочих правил (это аварийный случай, не плановая ротация).
    let badly_lossy: Vec<HoleId> = holes.iter().filter(|h| is_live(h) && is_badly_lossy(h, policy)).map(|h| h.id).collect();
    if !badly_lossy.is_empty() {
        let mut actions = Vec::with_capacity(badly_lossy.len() * 2);
        for id in badly_lossy {
            actions.push(Action::Retire(id));
            actions.push(Action::Open);
        }
        return actions;
    }

    let total_count = holes.len();

    // 3. Просроченная дыра — сливаем, только если это не уронит набор ниже `min_active`: слив
    // безопасен ровно настолько, насколько живых дыр сейчас больше минимума («slack»). Это и есть
    // практический смысл «прогретой замены» из дизайна — не по формальному наличию ЕЩЁ одной
    // Warming/Active дыры (их может не хватать всем сразу), а по фактическому запасу сверх
    // минимума. При нехватке запаса на всех просроченных — сливаем самых старых по возрасту
    // первыми, остальные ждут следующей оценки (запас появится от правила 2 или от собственного
    // старения других дыр).
    // Запас считаем по `Active`: прогреваемая замена в работу ещё не вошла, и слив старой дыры под
    // неё уронил бы набор в работе ниже минимума, пока замена не прогреется.
    let active_count = holes.iter().filter(|h| h.state == HoleState::Active).count();
    let slack = active_count.saturating_sub(policy.min_active);
    if slack > 0 {
        let mut expired: Vec<&HoleInfo> = holes.iter().filter(|h| is_live(h) && h.age >= h.max_age).collect();
        if !expired.is_empty() {
            expired.sort_by_key(|h| std::cmp::Reverse(h.age));
            return expired.into_iter().take(slack).map(|h| Action::Retire(h.id)).collect();
        }
    }

    // 2. Расти к max_total — по одной дыре за `add_interval`, не чаще.
    if total_count < policy.max_total && since_last_open >= policy.add_interval {
        return vec![Action::Open];
    }

    // 4. Дыр больше минимума — можно почистить худшую по потерям (при равных — самую старую), пусть
    // даже она не просрочена и не однозначно плохая, но только если потери заметные (выше
    // `cull_loss`). Только если это не уронит набор в работе ниже минимума.
    if active_count > policy.min_active {
        let candidate = holes
            .iter()
            .filter(|h| h.state == HoleState::Active && h.age >= policy.grace)
            .max_by(|a, b| worst_loss(a).total_cmp(&worst_loss(b)).then(a.age.cmp(&b.age)));
        if let Some(worst) = candidate
            && worst_loss(worst) > policy.cull_loss {
                return vec![Action::Retire(worst.id)];
            }
    }

    Vec::new()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hole(id: HoleId, state: HoleState, age_s: u64, max_age_s: u64, loss_out: Option<f32>, loss_in: Option<f32>) -> HoleInfo {
        HoleInfo { id, state, age: Duration::from_secs(age_s), max_age: Duration::from_secs(max_age_s), loss_out, loss_in }
    }

    fn active(id: HoleId, age_s: u64) -> HoleInfo {
        hole(id, HoleState::Active, age_s, 120, None, None)
    }

    #[test]
    fn empty_pool_opens_min_active_at_once() {
        let actions = plan(&[], Duration::ZERO, &PoolPolicy::default());
        assert_eq!(actions, vec![Action::Open; 4]);
    }

    #[test]
    fn below_minimum_tops_up_and_does_nothing_else_this_cycle() {
        let holes: Vec<_> = (0..3).map(|i| active(i, 20)).collect();
        let actions = plan(&holes, Duration::from_secs(999), &PoolPolicy::default());
        assert_eq!(actions, vec![Action::Open], "3 живых, нужно 4 — добираем 1");
    }

    #[test]
    fn never_drops_below_min_active_even_with_an_expired_hole() {
        // Ровно 4 дыры (= min_active), одна просрочена: запаса («slack») нет вообще, значит слить
        // просроченную сейчас нельзя — это уронило бы живых ниже минимума.
        let mut holes: Vec<_> = (0..4).map(|i| active(i, 10)).collect();
        holes[0].age = Duration::from_secs(200);
        holes[0].max_age = Duration::from_secs(120);
        let actions = plan(&holes, Duration::from_secs(999), &PoolPolicy::default());
        assert!(
            !actions.contains(&Action::Retire(0)),
            "слив единственной просроченной дыры на самом минимуме уронил бы набор ниже {min}: {actions:?}",
            min = 4
        );
    }

    #[test]
    fn grows_by_one_per_interval_up_to_max_total() {
        let holes: Vec<_> = (0..5).map(|i| active(i, 20)).collect();
        let policy = PoolPolicy::default();
        assert_eq!(plan(&holes, Duration::from_secs(5), &policy), Vec::new(), "интервал ещё не прошёл");
        assert_eq!(plan(&holes, Duration::from_secs(10), &policy), vec![Action::Open], "интервал прошёл — растим на одну");
    }

    #[test]
    fn stops_growing_at_max_total() {
        let holes: Vec<_> = (0..10).map(|i| active(i, 20)).collect();
        let actions = plan(&holes, Duration::from_secs(999), &PoolPolicy::default());
        assert!(!actions.contains(&Action::Open), "уже max_total — не растим дальше: {actions:?}");
    }

    #[test]
    fn with_eleven_holes_the_worst_by_loss_is_retired_ties_broken_by_age() {
        let policy = PoolPolicy::default();
        let mut holes: Vec<_> = (0..11).map(|i| active(i, 50)).collect();
        // Две дыры с одинаковой (худшей) долей потерь — при равенстве уходит самая старая.
        holes[3].loss_out = Some(0.2);
        holes[3].age = Duration::from_secs(80);
        holes[7].loss_out = Some(0.2);
        holes[7].age = Duration::from_secs(200); // старше — должна уйти она
        holes[7].max_age = Duration::from_secs(300); // но не просрочена — проверяем именно правило 4, не 3
        holes[9].loss_in = Some(0.05); // хуже нуля, но не худшая

        let actions = plan(&holes, Duration::from_secs(999), &policy);
        assert_eq!(actions, vec![Action::Retire(7)], "из двух равных по потерям уходит старшая");
    }

    #[test]
    fn expired_hole_is_retired_only_when_a_warm_replacement_exists() {
        let policy = PoolPolicy::default();
        // 6 дыр (выше минимума), одна просрочена, остальные — не просрочены (годятся «заменой»).
        let mut holes: Vec<_> = (0..6).map(|i| active(i, 20)).collect();
        holes[0].age = Duration::from_secs(130);
        holes[0].max_age = Duration::from_secs(120);
        let actions = plan(&holes, Duration::from_secs(999), &policy);
        assert_eq!(actions, vec![Action::Retire(0)], "замена есть (остальные 5 — Active) — сливаем просроченную");
    }

    #[test]
    fn expired_hole_without_any_replacement_is_kept() {
        let policy = PoolPolicy::default();
        // Ровно min_active дыр, все просрочены одновременно (например, одновременно открыты
        // давным-давно) — ни у одной нет небросившей замены среди остальных.
        let mut holes: Vec<_> = (0..4).map(|i| active(i, 200)).collect();
        for h in &mut holes {
            h.max_age = Duration::from_secs(120);
        }
        let actions = plan(&holes, Duration::from_secs(0), &policy);
        assert!(actions.iter().all(|a| !matches!(a, Action::Retire(_))), "ни одной замены ни у кого — никого не сливаем: {actions:?}");
    }

    #[test]
    fn a_badly_lossy_hole_is_retired_immediately_and_replaced() {
        let policy = PoolPolicy::default();
        let mut holes: Vec<_> = (0..6).map(|i| active(i, 50)).collect();
        holes[2].loss_in = Some(0.6); // хуже bad_loss=0.5 — доставлено меньше половины
        let actions = plan(&holes, Duration::from_secs(0), &policy);
        assert_eq!(actions, vec![Action::Retire(2), Action::Open], "аварийная плохая дыра: слив + немедленная замена");
    }

    #[test]
    fn a_fresh_hole_gets_grace_before_being_judged_on_loss() {
        let policy = PoolPolicy::default();
        let mut holes: Vec<_> = (0..6).map(|i| active(i, 50)).collect();
        holes[2] = hole(2, HoleState::Active, 1, 120, None, Some(0.9)); // только что открылась, уже видела потери
        let actions = plan(&holes, Duration::from_secs(999), &policy);
        assert!(!actions.contains(&Action::Retire(2)), "свежая дыра (моложе grace) не судится по потерям: {actions:?}");
    }

    #[test]
    fn warming_holes_count_toward_the_minimum() {
        // Прогреваемая (Warming) дыра уже считается «живой» для правила 1 (и для slack правила 3):
        // 3 Active + 1 Warming = 4 живых, ровно минимум — расти ещё рано, интервал роста не прошёл
        // (since_last_open = 0).
        let mut holes: Vec<_> = (0..3).map(|i| active(i, 20)).collect();
        holes.push(hole(3, HoleState::Warming, 1, 120, None, None));
        let actions = plan(&holes, Duration::ZERO, &PoolPolicy::default());
        assert_eq!(actions, Vec::new(), "3 Active + 1 Warming = 4 живых — ровно минимум, расти ещё рано");
    }

    #[test]
    fn draining_holes_do_not_count_as_live_and_are_never_targeted_again() {
        let mut holes: Vec<_> = (0..5).map(|i| active(i, 20)).collect();
        holes.push(hole(5, HoleState::Draining, 50, 120, Some(0.9), None)); // сливается, плохая, но уже не трогаем
        let actions = plan(&holes, Duration::from_secs(999), &PoolPolicy::default());
        assert_eq!(actions, vec![Action::Open], "Draining не считается живой — живых 5, растём по интервалу");
    }

    #[test]
    fn expired_hole_stays_until_its_replacement_has_warmed_up() {
        // Ровно `min_active` в работе, одна просрочена, замена ещё прогревается: сливать рано —
        // в работе осталось бы меньше минимума.
        let mut holes: Vec<HoleInfo> = (0..4).map(|i| active(i, if i == 0 { 200 } else { 30 })).collect();
        holes.push(hole(4, HoleState::Warming, 1, 120, None, None));
        assert_eq!(plan(&holes, Duration::ZERO, &PoolPolicy::default()), Vec::new());
        // Замена прогрелась — теперь можно.
        holes[4].state = HoleState::Active;
        assert_eq!(plan(&holes, Duration::ZERO, &PoolPolicy::default()), vec![Action::Retire(0)]);
    }

    #[test]
    fn small_losses_do_not_cull_a_hole_but_noticeable_ones_do() {
        let policy = PoolPolicy::default();
        let mut holes: Vec<HoleInfo> = (0..6).map(|i| active(i, 30)).collect();
        holes[3].loss_in = Some(0.02); // случайная потеря: ниже порога чистки
        assert_eq!(plan(&holes, Duration::ZERO, &policy), Vec::new(), "потери 2% — не повод менять дыру");
        holes[3].loss_in = Some(0.05);
        assert_eq!(plan(&holes, Duration::ZERO, &policy), vec![Action::Retire(3)], "потери 5% — меняем");
    }
}
