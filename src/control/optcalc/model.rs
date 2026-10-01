//! Option pricing model of the reference: closed form for European
//! exercise, a 100-step binomial tree with discrete dividends for American
//! exercise, the greeks of each, and the implied volatility solver.
//!
//! Every formula, constant and branch follows the reference so that the same
//! inputs give the same numbers to the last bits.

/// Length of a year in milliseconds (365 days).
pub const YEAR_MS: f64 = 3.1536e10;
/// One day as a year fraction.
pub const ONE_DAY_T: f64 = 0.0027397260273972603;
/// 16 hours as a year fraction: an expiry date with no time ends at 16:00.
pub const CLOSE_T: f64 = 0.0018264840182648401;
/// Shortest time to expiry (10 minutes) for an option not yet expired.
pub const MIN_T: f64 = 1.9025875190258754E-5;
/// Steps of the binomial tree.
pub const TREE_STEPS: usize = 100;
/// First guess of the implied volatility solver.
pub const IV_GUESS: f64 = 0.16;
/// Bounds of the delta stored in the greeks.
const DELTA_MAX: f64 = 0.999999999999999;
const DELTA_MIN: f64 = -0.999999999999999;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Right {
    Call,
    Put,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Style {
    American,
    European,
}

/// Time to expiry used by the model: the year fraction between the model
/// clock and the expiry, plus 16 hours when the expiry has no time of day;
/// below 10 minutes it is 10 minutes, and 0 once expired.
pub fn time_to_expiry_close(years: f64, add_close: bool) -> f64 {
    let t = if add_close { years + CLOSE_T } else { years };
    if t < MIN_T {
        if t < 0.0 { 0.0 } else { MIN_T }
    } else {
        t
    }
}

/// Year fraction between two instants in milliseconds.
pub fn years_between_ms(from_ms: i64, to_ms: i64) -> f64 {
    (to_ms - from_ms) as f64 / YEAR_MS
}

/// Year fraction rounded up to whole days.
pub fn years_integer_days(years: f64) -> f64 {
    (years * 365.0).ceil() / 365.0
}

/// Greeks of one model result. `NaN` is "not computed".
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Greeks {
    pub delta: f64,
    pub gamma: f64,
    pub vega: f64,
    pub theta: f64,
    /// Forward (or reference) underlying price of the result.
    pub forward: f64,
}

impl Default for Greeks {
    fn default() -> Self {
        Greeks { delta: f64::NAN, gamma: f64::NAN, vega: f64::NAN, theta: f64::NAN, forward: f64::NAN }
    }
}

impl Greeks {
    /// Store a delta, held inside (-1, 1).
    fn set_delta(&mut self, d: f64) {
        self.delta = if d > DELTA_MAX {
            DELTA_MAX
        } else if d < DELTA_MIN {
            DELTA_MIN
        } else {
            d
        };
    }
}

// ── Normal distribution (polynomial approximation of the reference) ──


fn norm_pdf(x: f64) -> f64 {
    (1.0 / (std::f64::consts::PI * 2.0).sqrt()) * (-x * x * 0.5).exp()
}

fn poly_t(x: f64) -> f64 {
    1.0 / (1.0 + 0.2316419 * x)
}

fn poly(t: f64) -> f64 {
    ((((1.330274429 * t + -1.821255978) * t + 1.781477937) * t + -0.356563782) * t + 0.31938153) * t
}

/// Cumulative normal distribution, 5-term polynomial approximation.
pub fn norm_cdf(x: f64) -> f64 {
    let n = norm_pdf(x);
    if x >= 0.0 {
        1.0 - n * poly(poly_t(x))
    } else {
        n * poly(poly_t(-x))
    }
}

fn d1_d2(log_moneyness: f64, carry: f64, vol: f64, t: f64) -> (f64, f64) {
    let sq = t.sqrt();
    let vs = vol * sq;
    let d1 = (log_moneyness + carry * t + vol * vol * 0.5 * t) / vs;
    (d1, d1 - vs)
}

// ── European closed form ──
// Arguments: s underlying, pv present value of the dividends, k strike,
// r rate, t time, vol volatility, q yield (carry).

/// European call price.
pub fn european_call(s: f64, pv: f64, k: f64, r: f64, t: f64, vol: f64, q: f64) -> f64 {
    let sx = s - pv;
    let disc = (-r * t).exp();
    let f = (((r - q) * t).exp()) * sx;
    let price = if vol == 0.0 {
        disc * (f - k)
    } else {
        let (d1, d2) = d1_d2((sx / k).ln(), r - q, vol, t);
        disc * (f * norm_cdf(d1) - k * norm_cdf(d2))
    };
    if price < 0.0 { 0.0 } else { price }
}

/// European put price.
pub fn european_put(s: f64, pv: f64, k: f64, r: f64, t: f64, vol: f64, q: f64) -> f64 {
    let sx = s - pv;
    let disc = (-r * t).exp();
    let f = sx * ((r - q) * t).exp();
    let price = if vol == 0.0 {
        disc * (k - f)
    } else {
        let (d1, d2) = d1_d2((sx / k).ln(), r - q, vol, t);
        disc * (k * norm_cdf(-d2) - f * norm_cdf(-d1))
    };
    if price < 0.0 { 0.0 } else { price }
}

/// European price with its greeks.
pub fn european_with_greeks(
    right: Right, s: f64, pv: f64, k: f64, r: f64, t: f64, vol: f64, q: f64, g: &mut Greeks,
) -> f64 {
    let call = right == Right::Call;
    let sx = s - pv;
    let disc = (-r * t).exp();
    let fwd_factor = ((r - q) * t).exp();
    let qd = (-q * t).exp();
    let kd = k * disc;
    let (d1, d2) = d1_d2((sx / k).ln(), r - q, vol, t);
    let n1 = norm_cdf(d1);
    let n2 = norm_cdf(d2);
    let (cn1, cn2) = if call { (n1, n2) } else { (1.0 - n1, 1.0 - n2) };
    let f = fwd_factor * sx;
    let (mut price, mut delta, mut gamma, mut vega, mut theta) = (0.0, 0.0, 0.0, 0.0, 0.0);
    if vol == 0.0 {
        price = if call { disc * (f - k) } else { disc * (k - f) };
        if price > 0.0 {
            if call {
                delta = qd;
                theta = (qd * q * sx - r * kd) / 365.0;
            } else {
                delta = -qd;
                theta = (-qd * q * sx + r * kd) / 365.0;
            }
        }
    } else {
        price = if call { disc * (f * cn1 - k * cn2) } else { disc * (k * cn2 - f * cn1) };
        let sq = t.sqrt();
        delta = if call { cn1 * qd } else { -cn1 * qd };
        let pdf = norm_pdf(d1);
        gamma = pdf * qd / (sx * vol * sq);
        vega = sx * sq * pdf * qd * 0.01;
        let t1 = sx * pdf * vol / (2.0 * sq);
        let t2 = r * kd * cn2;
        theta = if call {
            if q == 0.0 { (-t1 - t2) / 365.0 } else { (qd * (-t1 + q * sx * cn1) - t2) / 365.0 }
        } else if q == 0.0 {
            (-t1 + t2) / 365.0
        } else {
            (qd * (-t1 - q * sx * cn1) + t2) / 365.0
        };
    }
    g.set_delta(delta);
    g.gamma = gamma;
    g.theta = theta;
    g.vega = vega;
    if price < 0.0 {
        price = 0.0;
    }
    g.forward = f;
    price
}

/// European vega for a 1.00 change of volatility, used as the slope of the
/// implied volatility solver for every exercise style.
pub fn european_vega(s: f64, pv: f64, k: f64, r: f64, q: f64, vol: f64, t: f64) -> f64 {
    let sx = s - pv;
    let (d1, _) = d1_d2((sx / k).ln(), r - q, vol, t);
    let v = sx * t.sqrt() * norm_pdf(d1) / 100.0;
    (-q * t).exp() * v
}

// ── Binomial tree ──

/// A discrete dividend as the tree needs it, relative to the model clock.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct TreeDividend {
    /// Year fraction from the clock to the ex-date (exact).
    pub years: f64,
    /// Same, rounded up to whole days.
    pub years_int: f64,
    /// Whole days from the clock to the end of the ex-date (local day).
    pub days: i64,
    /// Cash amount (already multiplied by the tax ratio when one applies).
    pub amount: f64,
}

struct TreeArrays {
    stock: Vec<f64>,
    value: Vec<f64>,
    divs: Vec<f64>,
}

impl TreeArrays {
    fn new(n: usize) -> Self {
        TreeArrays { stock: vec![0.0; n + 1], value: vec![0.0; n + 1], divs: vec![0.0; n + 1] }
    }
}

/// Fill the escrowed dividend value of each step; returns the largest one.
/// `index` takes the amounts as cumulative (index dividends).
fn dividend_steps(divs: &[TreeDividend], arr: &mut [f64], index: bool, r: f64, q: f64, t: f64) -> f64 {
    if divs.is_empty() {
        return 0.0;
    }
    let n = arr.len() - 1;
    let dt = t / n as f64;
    let growth = ((r - q) * dt).exp();
    let mut prev_amount = 0.0;
    let mut max_value = 0.0;
    for d in divs {
        let in_life = t + 0.0 >= d.years;
        let after_today = d.days > 0;
        if !(in_life && after_today) {
            continue;
        }
        let mut amount = d.amount;
        if index {
            amount -= prev_amount;
            prev_amount = d.amount;
        }
        let mut disc = (-r * d.years_int).exp();
        let mut step_t = 0.0;
        for slot in arr.iter_mut().take(n) {
            if step_t > d.years_int {
                break;
            }
            *slot += amount * disc;
            step_t += dt;
            disc *= growth;
            if max_value < *slot {
                max_value = *slot;
            }
        }
        arr[n] = 0.0;
    }
    max_value
}

fn tree_terminal(a: &mut TreeArrays, s: f64, k: f64, u: f64, sign: f64) {
    let n = a.value.len() - 1;
    let u2 = u * u;
    let len = a.stock.len();
    let mid = len / 2;
    a.stock[mid] = s - a.divs[0];
    if len % 2 == 0 {
        a.stock[mid] *= u;
    }
    for i in mid + 1..len {
        a.stock[i] = a.stock[i - 1] * u2;
    }
    for i in (0..mid).rev() {
        a.stock[i] = a.stock[i + 1] / u2;
    }
    let k_end = k - a.divs[n];
    for i in 0..len {
        let v = (a.stock[i] - k_end) * sign;
        a.value[i] = if v > 0.0 { v } else { 0.0 };
    }
}

/// Insertion point of `key` in the sorted terminal prices (first index with
/// a price not below the key).
fn strike_index(stock: &[f64], k: f64, max_div: f64, sign: f64) -> usize {
    let key = if sign > 0.0 { k - max_div } else { k };
    let mut lo: isize = 0;
    let mut hi: isize = stock.len() as isize - 1;
    while lo <= hi {
        let mid = (lo + hi) >> 1;
        let v = stock[mid as usize];
        if v < key {
            lo = mid + 1;
        } else if v > key {
            hi = mid - 1;
        } else {
            return mid as usize;
        }
    }
    lo as usize
}

#[allow(clippy::too_many_arguments)]
fn tree_back_call(
    american: bool, s: f64, k: f64, max_div: f64, r: f64, t: f64,
    a: &mut TreeArrays, p: f64, u: f64, mut g: Option<&mut Greeks>,
) {
    let n = a.value.len() - 1;
    let dt = t / n as f64;
    let disc = (-r * dt).exp();
    let lo = strike_index(&a.stock, k, max_div, 1.0) as isize - 1;
    let q = 1.0 - p;
    let dinv = 1.0 / u;
    let s0 = s - a.divs[0];
    let mut mid_level2 = f64::NAN;
    for j in (1..=n).rev() {
        let kj = k - a.divs[j - 1];
        let start = if (n - j) as isize > lo { (n - j) as isize } else { lo };
        let mut i = n as isize;
        while i > start {
            let iu = i as usize;
            let mut val = (p * a.value[iu] + q * a.value[iu - 1]) * disc;
            if max_div > 0.0 {
                a.stock[iu] *= dinv;
                let ex = a.stock[iu] - kj;
                if val < ex && american {
                    val = ex;
                }
            }
            a.value[iu] = val;
            i -= 1;
        }
        if let Some(g) = g.as_deref_mut() {
            if j == 2 {
                g.set_delta((a.value[n] - a.value[n - 1]) / (s0 * (u - dinv)));
            }
            if j == 3 {
                mid_level2 = a.value[n - 1];
                let u2 = u * u;
                let d2 = dinv * dinv;
                let h1 = 1.0 / (s0 * (u2 - 1.0));
                let h2 = 1.0 / (s0 * (1.0 - d2));
                let hh = 0.5 * s0 * (u2 - d2);
                g.gamma = ((a.value[n] - a.value[n - 1]) * h1 - (a.value[n - 1] - a.value[n - 2]) * h2) / hh;
            }
        }
    }
    if let Some(g) = g {
        g.theta = (mid_level2 - a.value[n]) / (2.0 * dt * 365.0);
    }
}

#[allow(clippy::too_many_arguments)]
fn tree_back_put(
    american: bool, s: f64, k: f64, max_div: f64, r: f64, t: f64,
    a: &mut TreeArrays, p: f64, u: f64, mut g: Option<&mut Greeks>,
) {
    let n = a.value.len() - 1;
    let dt = t / n as f64;
    let disc = (-r * dt).exp();
    let hi = strike_index(&a.stock, k, max_div, -1.0) + 1;
    let q = 1.0 - p;
    let dinv = 1.0 / u;
    let s0 = s - a.divs[0];
    let mut mid_level2 = f64::NAN;
    for j in (1..=n).rev() {
        let kj = k - a.divs[j - 1];
        let end = if j < hi { j } else { hi };
        for i in 0..end {
            let mut val = (p * a.value[i + 1] + q * a.value[i]) * disc;
            a.stock[i] *= u;
            let ex = kj - a.stock[i];
            if val < ex && american {
                val = ex;
            }
            a.value[i] = val;
        }
        if let Some(g) = g.as_deref_mut() {
            if j == 3 {
                let u2 = u * u;
                let d2 = dinv * dinv;
                let h1 = 1.0 / (s0 * (u2 - 1.0));
                let h2 = 1.0 / (s0 * (1.0 - d2));
                let hh = 0.5 * s0 * (u2 - d2);
                g.gamma = ((a.value[2] - a.value[1]) * h1 - (a.value[1] - a.value[0]) * h2) / hh;
                mid_level2 = a.value[1];
            }
            if j == 2 {
                g.set_delta((a.value[1] - a.value[0]) / (s0 * (u - dinv)));
            }
        }
    }
    if let Some(g) = g {
        g.theta = (mid_level2 - a.value[0]) / (2.0 * dt * 365.0);
    }
}

/// Inputs of one tree valuation.
#[derive(Debug, Clone, Copy)]
pub struct TreeInput<'a> {
    pub right: Right,
    pub american: bool,
    pub s: f64,
    pub k: f64,
    pub r: f64,
    pub q: f64,
    pub t: f64,
    pub vol: f64,
    pub dividends: &'a [TreeDividend],
    pub index: bool,
}

/// Tree price; fills delta, gamma and theta when `g` is given.
pub fn tree_price(inp: &TreeInput, steps: usize, g: Option<&mut Greeks>) -> f64 {
    let sign = if inp.right == Right::Put { -1.0 } else { 1.0 };
    let mut a = TreeArrays::new(steps);
    let dt = inp.t / steps as f64;
    let sq = dt.sqrt();
    let mut u = (inp.vol * sq).exp();
    let d = 1.0 / u;
    let growth = ((inp.r - inp.q) * dt).exp();
    let mut p = (growth - d) / (u - d);
    if p > 1.0 {
        u = growth;
        p = 1.0;
    }
    let max_div = dividend_steps(inp.dividends, &mut a.divs, inp.index, inp.r, inp.q, inp.t);
    tree_terminal(&mut a, inp.s, inp.k, u, sign);
    if sign > 0.0 {
        tree_back_call(inp.american, inp.s, inp.k, max_div, inp.r, inp.t, &mut a, p, u, g);
        a.value[steps]
    } else {
        tree_back_put(inp.american, inp.s, inp.k, max_div, inp.r, inp.t, &mut a, p, u, g);
        a.value[0]
    }
}

/// Tree price with delta, gamma, theta, and vega as the price change for a
/// volatility 0.01 higher.
pub fn tree_with_greeks(inp: &TreeInput, steps: usize, g: &mut Greeks) -> f64 {
    let price = tree_price(inp, steps, Some(g));
    let bumped = TreeInput { vol: inp.vol + 0.01, ..*inp };
    let up = tree_price(&bumped, steps, None);
    g.vega = up - price;
    price
}

// ── Option model ──

/// One option valuation: the contract terms and the market inputs.
#[derive(Debug, Clone)]
pub struct OptionModel {
    pub right: Right,
    pub style: Style,
    /// Underlying price.
    pub s: f64,
    pub strike: f64,
    /// Time to expiry (year fraction).
    pub t: f64,
    /// Present value of the dividends paid before expiry (closed form).
    pub pv_dividend: f64,
    /// Continuous yield of the underlying (carry).
    pub q: f64,
    /// Continuous interest rate.
    pub r: f64,
    /// Discrete dividends (tree).
    pub dividends: Vec<TreeDividend>,
    /// Index underlying: tree dividend amounts are cumulative.
    pub index: bool,
}

impl OptionModel {
    fn intrinsic_forward(&self) -> f64 {
        let df = if self.q == 0.0 { 1.0 } else { (-self.q * self.t).exp() };
        let v = self.s * df - self.pv_dividend;
        if v > 0.0 { v } else { 0.0 }
    }

    /// Value with no volatility (or no underlying price).
    fn intrinsic(&self, g: Option<&mut Greeks>) -> f64 {
        let fwd = self.intrinsic_forward();
        let kd = self.strike * (-self.r * self.t).exp();
        let call = self.right == Right::Call;
        let mut v = if call { (fwd - kd).max(0.0) } else { (kd - fwd).max(0.0) };
        if self.style == Style::American {
            let ex = self.s - self.strike;
            v = if call { v.max(ex) } else { v.max(-ex) };
        }
        if let Some(g) = g {
            let df = if self.q == 0.0 { 1.0 } else { (-self.q * self.t).exp() };
            g.vega = 0.0;
            g.gamma = 0.0;
            if v > 0.0 {
                if call {
                    g.set_delta(df);
                    g.theta = (self.q * self.s * df - self.r * kd) / 365.0;
                } else {
                    g.set_delta(-df);
                    g.theta = 0.0;
                }
            } else {
                g.delta = 0.0;
                g.theta = 0.0;
            }
        }
        v
    }

    fn tree_input(&self, vol: f64) -> TreeInput<'_> {
        TreeInput {
            right: self.right, american: true, s: self.s, k: self.strike, r: self.r, q: self.q,
            t: self.t, vol, dividends: &self.dividends, index: self.index,
        }
    }

    /// Model price at a volatility (no greeks).
    pub fn price(&self, vol: f64) -> f64 {
        if vol == 0.0 || self.s <= 0.0 {
            return self.intrinsic(None);
        }
        match self.style {
            Style::European => match self.right {
                Right::Call => european_call(self.s, self.pv_dividend, self.strike, self.r, self.t, vol, self.q),
                Right::Put => european_put(self.s, self.pv_dividend, self.strike, self.r, self.t, vol, self.q),
            },
            Style::American => {
                let inp = TreeInput { american: true, ..self.tree_input(vol) };
                tree_price(&inp, TREE_STEPS, None)
            }
        }
    }

    /// Model price at a volatility with its greeks.
    pub fn price_with_greeks(&self, vol: f64) -> (f64, Greeks) {
        let mut g = Greeks::default();
        if vol == 0.0 || self.s <= 0.0 {
            let p = self.intrinsic(Some(&mut g));
            return (p, g);
        }
        let p = match self.style {
            Style::European => european_with_greeks(
                self.right, self.s, self.pv_dividend, self.strike, self.r, self.t, vol, self.q, &mut g,
            ),
            Style::American => tree_with_greeks(&self.tree_input(vol), TREE_STEPS, &mut g),
        };
        (p, g)
    }

    /// Implied volatility of an option price: the solver of the reference
    /// from `guess`; `None` when it does not converge.
    pub fn implied_vol(&self, option_price: f64, guess: f64) -> Option<f64> {
        let f = |v: f64| self.price(v) - option_price;
        let slope = |v: f64| european_vega(self.s, self.pv_dividend, self.strike, self.r, self.q, v, self.t) * 100.0;
        solve(f, slope, guess, &SolverSettings::default())
    }
}

/// Settings of the implied volatility solver.
pub struct SolverSettings {
    pub max_iter: usize,
    pub tolerance: f64,
    pub max_step: f64,
    pub lower: f64,
    pub upper: f64,
}

impl Default for SolverSettings {
    fn default() -> Self {
        SolverSettings { max_iter: 20, tolerance: 1.0E-6, max_step: 1.6, lower: 0.0, upper: f64::MAX }
    }
}

/// Root of `f` from `x0`: Newton steps on the given slope first, secant
/// steps after, with step limits and halving, as the reference solver.
pub fn solve(f: impl Fn(f64) -> f64, slope_fn: impl Fn(f64) -> f64, x0: f64, s: &SolverSettings) -> Option<f64> {
    if x0.is_nan() {
        return None;
    }
    const UNSET: f64 = f64::MAX;
    let mut x = x0;
    let mut fx = f(x);
    let mut x_prev = UNSET;
    let mut f_prev = UNSET;
    let mut slope = UNSET;
    let mut max_step = s.max_step;
    // 596 running, 0 converged, 1 too many iterations, 2 no slope, 3 stuck.
    let mut status = 596;
    let mut it = 0;
    let mut step;
    while it < s.max_iter {
        if fx.abs() < s.tolerance {
            status = 0;
            if slope == UNSET {
                let _ = slope_fn(x);
            }
            break;
        }
        if x_prev == UNSET || (x_prev - x).abs() < 1.0E-14 {
            slope = slope_fn(x);
        } else {
            slope = (fx - f_prev) / (x - x_prev);
        }
        if slope < f64::from_bits(1) {
            status = 2;
            step = -max_step * fx.abs() / fx;
        } else {
            step = -fx / slope;
            if step.abs() > max_step {
                step *= max_step / step.abs();
            }
        }
        x_prev = x;
        f_prev = fx;
        if step > s.upper - x_prev {
            step = s.upper - x_prev;
        }
        if step < s.lower - x_prev {
            step = s.lower - x_prev;
        }
        if step.abs() <= 1.0E-16 {
            status = 3;
            break;
        }
        for _ in 0..10 {
            let xn = x_prev + step;
            let fnew = f(xn);
            if fnew.abs() < f_prev.abs() {
                x = xn;
                fx = fnew;
                break;
            }
            step *= 0.5;
        }
        if x_prev == x {
            status = if fx.abs() < s.tolerance { 0 } else { 3 };
            break;
        }
        if status == 2 {
            max_step = step.abs();
            it += 1;
            continue;
        }
        let ratio = (fx - f_prev) / (slope * step);
        if ratio < 0.1 {
            max_step *= 0.25;
        } else if ratio > 0.7 && step.abs() - max_step < 1.0E-14 {
            max_step = s.max_step.max(2.0 * step.abs());
        }
        it += 1;
    }
    if it >= s.max_iter {
        status = 1;
    }
    if status != 0 || x.is_infinite() || x == f64::MAX {
        None
    } else {
        Some(x)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Capture #40 (AAPL 20260306 255 C, conId 848313396), price request at
    // 04/03/2026 10:18:21.560 CET, IV request at 10:18:09.554 CET. The model
    // clock of the reference ticks once a minute; both requests used the
    // tick of 10:17:36.422 CET. The rate is the one that reproduces all five
    // captured values; no dividend falls before the expiry.
    const CAPTURE_T: f64 = 0.006814384830035515 + 45.13799976130771 / 3.1536e7;
    const CAPTURE_R: f64 = 0.03949488153593471;

    fn capture_model() -> OptionModel {
        OptionModel {
            right: Right::Call, style: Style::American, s: 250.0, strike: 255.0, t: CAPTURE_T,
            pv_dividend: 0.0, q: 0.0, r: CAPTURE_R, dividends: Vec::new(), index: false,
        }
    }

    fn close(a: f64, b: f64, tol: f64) -> bool {
        (a - b).abs() <= tol * b.abs().max(1e-300)
    }

    #[test]
    fn time_to_expiry_adds_16_hours_and_floors() {
        assert_eq!(time_to_expiry_close(0.01, true), 0.01 + CLOSE_T);
        assert_eq!(time_to_expiry_close(0.01, false), 0.01);
        assert_eq!(time_to_expiry_close(1e-6, false), MIN_T);
        assert_eq!(time_to_expiry_close(-1e-6, false), 0.0);
    }

    #[test]
    fn norm_cdf_is_the_polynomial_approximation() {
        assert!((norm_cdf(0.0) - 0.5).abs() < 1e-9);
        assert!((norm_cdf(1.0) - 0.8413447).abs() < 1e-7);
        assert!((norm_cdf(-1.0) - 0.1586553).abs() < 1e-7);
        // The approximation is not exact: 1 - N(x) and N(-x) differ in the last bits.
        assert_ne!(norm_cdf(-1.3), 1.0 - norm_cdf(1.3));
    }

    #[test]
    fn capture_40_option_price_and_greeks() {
        let (price, g) = capture_model().price_with_greeks(0.3);
        assert!(close(price, 0.7612065964530287, 1e-9), "{price}");
        assert!(close(g.delta, 0.21766418914368796, 1e-9), "{}", g.delta);
        assert!(close(g.gamma, 0.04785750216838694, 1e-9), "{}", g.gamma);
        assert!(close(g.vega, 0.06577240653056571, 1e-9), "{}", g.vega);
        assert!(close(g.theta, -0.3745716294945733, 1e-9), "{}", g.theta);
    }

    #[test]
    fn capture_40_implied_volatility() {
        let iv = capture_model().implied_vol(5.0, IV_GUESS).unwrap();
        assert!(close(iv, 0.8640485765653304, 1e-9), "{iv}");
    }

    #[test]
    fn tree_without_dividends_call_equals_price_path() {
        let m = capture_model();
        let (with_greeks, _) = m.price_with_greeks(0.25);
        assert_eq!(m.price(0.25), with_greeks);
    }

    #[test]
    fn american_put_is_at_least_intrinsic() {
        let m = OptionModel { right: Right::Put, strike: 300.0, ..capture_model() };
        let p = m.price(0.3);
        assert!(p >= 50.0, "{p}");
        let (pg, g) = m.price_with_greeks(0.3);
        assert_eq!(p, pg);
        assert!(g.delta < 0.0 && g.delta >= -0.999999999999999);
    }

    #[test]
    fn european_put_call_parity() {
        let (s, k, r, t, v) = (100.0, 95.0, 0.03, 0.5, 0.2);
        let c = european_call(s, 0.0, k, r, t, v, 0.0);
        let p = european_put(s, 0.0, k, r, t, v, 0.0);
        assert!((c - p - (s - k * (-r * t).exp())).abs() < 1e-5);
        let mut g = Greeks::default();
        let cg = european_with_greeks(Right::Call, s, 0.0, k, r, t, v, 0.0, &mut g);
        assert!((cg - c).abs() < 1e-12);
        assert!(g.delta > 0.5 && g.delta < 1.0);
    }

    #[test]
    fn zero_volatility_gives_intrinsic_value() {
        let m = OptionModel { strike: 240.0, ..capture_model() };
        let (p, g) = m.price_with_greeks(0.0);
        let kd = 240.0 * (-CAPTURE_R * CAPTURE_T).exp();
        assert_eq!(p, (250.0f64 - kd).max(10.0));
        assert_eq!(g.vega, 0.0);
        assert_eq!(g.delta, 1.0f64.min(0.999999999999999));
    }

    #[test]
    fn dividend_lowers_the_american_call() {
        let base = capture_model();
        let with_div = OptionModel {
            dividends: vec![TreeDividend { years: 0.003, years_int: 2.0 / 365.0, days: 1, amount: 2.0 }],
            ..capture_model()
        };
        assert!(with_div.price(0.3) < base.price(0.3));
        // A dividend whose ex-date is today is not counted.
        let today = OptionModel {
            dividends: vec![TreeDividend { years: 0.0001, years_int: 1.0 / 365.0, days: 0, amount: 2.0 }],
            ..capture_model()
        };
        assert_eq!(today.price(0.3), base.price(0.3));
    }

    #[test]
    fn solver_reports_no_root() {
        // A price below the intrinsic value has no volatility.
        let m = OptionModel { strike: 200.0, ..capture_model() };
        assert_eq!(m.implied_vol(1.0, IV_GUESS), None);
    }
}
