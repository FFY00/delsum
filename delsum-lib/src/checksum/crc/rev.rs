//! This module contains the function(s) for reversing the parameters for a CRC algorithm with given bytes and checksums.
//!
//! Generally, to find out the parameters, the checksums and their width are needed, and 3 of the following (with at least one file):
//! * value of `init`
//! * value of `xorout`
//! * value of `module`
//! * a file with checksum
//! * a different file with checksum
//! * yet another different file checksum
//!
//! If `init` is not known, it is neccessary to know two checksums of files with different lengths.
//! In case only checksums of files with a set length are required, setting `init = 0` is sufficient.
use super::{CRCBuilder, CRC};
use crate::checksum::{unresult_iter, CheckReverserError};
use delsum_poly::*;
#[cfg(feature = "parallel")]
use rayon::prelude::*;
use std::convert::TryInto;
use std::pin::Pin;

/// Find the parameters of a CRC algorithm.
///
/// `spec` contains the known parameters of the algorithm (by setting the corresponding values in the builder).
/// `chk_bytes` are pairs of files and their checksums.
/// `verbosity` makes the function output what it is doing.
///
/// The `width` parameter of the builder has to be set.
pub fn reverse_crc<'a>(
    spec: &CRCBuilder<u128>,
    chk_bytes: &'a [(&[u8], u128)],
    verbosity: u64,
) -> impl Iterator<Item = Result<CRC<u128>, CheckReverserError>> + 'a {
    let spec = spec.clone();
    let ref_combinations: Vec<_> = ref_comb(spec.refin, spec.refout);
    ref_combinations
        .into_iter()
        .map(move |(refin, refout)| {
            unresult_iter(reverse(&spec, chk_bytes, verbosity, refin, refout))
        })
        .flatten()
}

/// Parallel version of reverse_crc.
///
/// Note that this is parallel only because it tries the 4 combinations of refin, refout
/// at once when not given, giving at most a 4x speedup.
#[cfg(feature = "parallel")]
pub fn reverse_crc_para<'a>(
    spec: &CRCBuilder<u128>,
    chk_bytes: &'a [(&[u8], u128)],
    verbosity: u64,
) -> impl ParallelIterator<Item = Result<CRC<u128>, CheckReverserError>> + 'a {
    let spec = spec.clone();
    let ref_combinations: Vec<_> = ref_comb(spec.refin, spec.refout);
    ref_combinations
        .into_par_iter()
        .map(move |(refin, refout)| {
            unresult_iter(reverse(&spec, chk_bytes, verbosity, refin, refout)).par_bridge()
        })
        .flatten()
}

// find all combinations of refin, refout using both bool values when a parameter is not given
fn ref_comb(maybe_refin: Option<bool>, maybe_refout: Option<bool>) -> Vec<(bool, bool)> {
    let refins = maybe_refin
        .map(|x| vec![x])
        .unwrap_or_else(|| vec![false, true]);
    let refouts = maybe_refout
        .map(|x| vec![x])
        .unwrap_or_else(|| vec![false, true]);
    refins
        .iter()
        .map(|&x| refouts.iter().map(move |&y| (x, y)))
        .flatten()
        .collect()
}

// wrapper to call rev_from_polys with polynomial arguments
fn reverse<'a>(
    spec: &CRCBuilder<u128>,
    chk_bytes: &'a [(&[u8], u128)],
    verbosity: u64,
    refin: bool,
    refout: bool,
) -> Result<impl Iterator<Item = CRC<u128>> + 'a, CheckReverserError> {
    let width = match spec.width {
        Some(x) => x,
        None => return Err(CheckReverserError::MissingParameter("width")),
    };
    // check for errors in the parameters
    if 3 > chk_bytes.len()
        + spec.init.is_some() as usize
        + spec.xorout.is_some() as usize
        + spec.poly.is_some() as usize
    {
        return Err(CheckReverserError::MissingParameter(
            "at least 3 parameters/files",
        ));
    }
    if spec.init.is_some()
        || chk_bytes.iter().map(|x| x.0.len()).max() == chk_bytes.iter().map(|x| x.0.len()).min()
    {
        return Err(CheckReverserError::UnsuitableFiles(
            "need at least one file with different length",
        ));
    }
    // convert the files to polynomials
    let mut polys: Vec<_> = chk_bytes
        .iter()
        .map(|(b, c)| (bytes_to_poly(b, *c, width as u8, refin, refout), b.len()))
        .collect();
    // sort by reverse file length
    polys.sort_by(|(fa, la), (fb, lb)| la.cmp(&lb).then(deg(fa).cmp(&deg(fb)).reverse()));
    // convert parameters to polynomials
    let revinfo = RevInfo::from_builder(spec, refin, refout);
    rev_from_polys(&revinfo, &polys, verbosity).map(|x| x.iter())
}

struct RevInfo {
    width: usize,
    init: Option<PolyPtr>,
    xorout: Option<PolyPtr>,
    poly: Option<PolyPtr>,
    refin: bool,
    refout: bool,
}

impl RevInfo {
    fn from_builder(spec: &CRCBuilder<u128>, refin: bool, refout: bool) -> Self {
        let width = spec.width.unwrap();
        let init = spec.init.map(|i| new_poly(&i.to_be_bytes()));
        let poly = spec.poly.map(|p| {
            let mut p = new_poly(&p.to_be_bytes());
            // add leading coefficient, which is omitted in binary form
            p.pin_mut()
                .add_to(&new_poly_shifted(&[1], width as i64, true));
            p
        });
        // while init and poly are unaffected by refout, xorout is not
        let xorout = spec
            .xorout
            .map(|x| new_poly(&cond_reverse(width as u8, x, refout).to_be_bytes()));
        RevInfo {
            width,
            init,
            xorout,
            poly,
            refin,
            refout,
        }
    }
}

struct RevResult {
    polys: Vec<PolyPtr>,
    inits: PrefactorMod,
    xorout: InitPoly,
    width: usize,
    refin: bool,
    refout: bool,
}

impl RevResult {
    // iterate over all possible parameters
    fn iter(self) -> impl Iterator<Item = CRC<u128>> {
        let RevResult {
            polys,
            inits,
            xorout,
            width,
            refin,
            refout,
        } = self;
        polys
            .into_iter()
            .map(move |pol| {
                // for each polynomial of degree width, iterate over all solutions of the PrefactorMod
                inits
                    .iter_inits(&pol, &xorout)
                    .map(move |(poly_p, init_p, xorout_p)| {
                        // convert polynomial parameters to a CRC<u128>
                        let poly = poly_to_u128(&add(
                            &poly_p,
                            &new_poly_shifted(&[1], width as i64, true),
                        ));
                        let init = poly_to_u128(&init_p);
                        let xorout = cond_reverse(width as u8, poly_to_u128(&xorout_p), refout);
                        CRC::<u128>::with_options()
                            .width(width)
                            .poly(poly)
                            .init(init)
                            .xorout(xorout)
                            .refin(refin)
                            .refout(refout)
                            .build()
                            .unwrap()
                    })
            })
            .flatten()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum InitPlace {
    None,
    Single(usize),
    Pair(usize, usize),
}

type InitPoly = (PolyPtr, InitPlace);

// The parameter reversing for CRC is quite similar and it may be easier to try to understand that implementation first,
// since it uses integers instead of 𝔽₂[X].
//
// If f is a file of length l (in bits) interpreted as a polynomial in 𝔽₂[X], then the crc is just
//  (init*X^l + f*X^width + xorout) % poly
//
// If we have a file with a crc checksum, we can calculate
//  checksum - f*X^width ≡ init*X^l + xorout mod poly
// Note that poly is not yet known, so we can't reduce by poly yet and have a giant degree l polynomial,
// with a file that is a few MB, this is a polynomial whose degree is a few millions, so each operation
// can be expensive.
//
// By using multiple files, we can also cancel xorout and init:
// Given three files of len l₁, l₂, l₃, we have calculated init*X^lₐ + xorout mod poly before, so by subtracting
// the first two, we get `a = init*(X^l₁ + X^l₂) mod poly`. Doing the 2nd and 3rd, we get similarly get `b = init*(X^l₂ + X^l₃) mod poly`.
// For simplicity, let's assume l₁ < l₂ < l₃ (if there are two of the same length, init is already cancelled).
// If we multiply a by (X^(l₃ - l₂) + 1), we get init*(X^(l₃ + l₁ - l₂) + X^l₃ + X^l₂ + X^l₁) mod poly.
// When we multiply b by (X^(l₂ - l₁) + 1), we also get that, so by subtracting both, we get 0 mod poly, meaning that
// poly divides the result, which we can use to determine poly later.
//
// If we have more than three files, we can also get more results, but since poly has to divide all of them, we can gcd them
// together to get a smaller polynomial that is divided by poly.
// If we don't have that, we still know that the highest prime factor of poly that we care about has degree width,
// which we can use to construct a polynomial that only has factors of degree <= width and gcd with that.
//
// One could think that doing a gcd between million-degree polynomials could be very slow.
// And if a naive implementation of multiplication and gcd were used, that would be correct.
// However this program uses two excellent libraries, NTL and gf2x, with which the gcd can be calculated in
// around O(n*log^2(n)) time, thanks to the FFT-based Schönhage-Strassen multiplication and a clever
// gcd implementation called half-gcd.
//
// Now we just assume that the result we got in the previous step is already our poly.
// We can just adjust it to be a divisor of that if we found it to be wrong later.
// With that, we can solve init*(X^l₂ + X^l₁) ≡ x mod poly for init using number theory®
// and from that, we get xorout by subtracting e.g. init*X^l₁.
//
// If our poly is still of degree higher than width, we can then factorize it.
// Note that factoring 𝔽₂[X] polynoials is suprisingly feasable (people have factored
// such polynomials in the degree of millions) and because the factors all have degree <= width,
// due to the way distinct degree factorization works, it should still work quite fast.
// However, by this point poly should be very close in degree to width, so it's not a very big issue anyway.
//
// Using the factorization, we can then iterate over all divisors of degree width.
fn rev_from_polys(
    spec: &RevInfo,
    arg_polys: &[(PolyPtr, usize)],
    verbosity: u64,
) -> Result<RevResult, CheckReverserError> {
    let log = |s| {
        if verbosity > 0 {
            eprintln!(
                "<crc, refin = {}, refout = {}> {}",
                spec.refin, spec.refout, s
            );
        }
    };
    // InitPlace is essentially a sparse polynomial with at most 2 coefficients being 1
    // note that it has an implied factor of 8, because it uses the byte position instead of bit position
    let mut polys: Vec<_> = arg_polys
        .iter()
        .rev()
        .map(|(p, l)| (copy_poly(p), InitPlace::Single(*l)))
        .collect();
    if let Some(init) = &spec.init {
        log("removing inits");
        remove_inits(&init, &mut polys);
    }
    log("removing xorouts");
    let (polys, mut xorout) = remove_xorouts(&spec.xorout, polys);
    log("finding poly");
    let (polys, mut hull) = find_polyhull(spec, polys, verbosity);
    log("finding init and refining poly");
    let init = find_init(&spec.init, hull.pin_mut(), polys);
    let polyhull_factors: Vec<_>;
    if deg(&hull) > 0 {
        xorout.0.pin_mut().rem_to(&hull);
        log("factoring poly");
        polyhull_factors = factor(&hull, if verbosity > 1 { 1 } else { 0 })
            .into_iter()
            .map(|PolyI64Pair { poly, l }| (copy_poly(poly), *l))
            .collect();
    } else {
        log("could not find any fitting factors for poly");
        xorout.0 = new_zero();
        polyhull_factors = vec![];
    }
    log("finding all factor combinations for poly and finishing");
    Ok(RevResult {
        polys: find_prod_comb(spec.width, &polyhull_factors),
        inits: init,
        xorout,
        width: spec.width,
        refin: spec.refin,
        refout: spec.refout,
    })
}

fn remove_inits(init: &Poly, polys: &mut [InitPoly]) {
    for (p, l) in polys {
        match l {
            InitPlace::Single(d) => {
                p.pin_mut().add_to(&shift(init, 8 * *d as i64));
                *l = InitPlace::None;
            }
            // note: this branch shouldn't happen, but it is also no problem if it happens
            InitPlace::None => (),
            // this is not really a problem either, but I will not bother implementing it
            InitPlace::Pair(_, _) => {
                panic!("Internal Error: remove_inits should not receive Pair Inits")
            }
        }
    }
}

fn remove_xorouts(
    maybe_xorout: &Option<PolyPtr>,
    mut polys: Vec<InitPoly>,
) -> (Vec<InitPoly>, InitPoly) {
    let mut ret_vec = Vec::new();
    let mut prev = polys
        .pop()
        .expect("Internal Error: Zero-length vector given to remove_xorouts");
    let xor_ret = match maybe_xorout {
        Some(xorout) => {
            // if we already have xorout, we can subtract it from the files themselves so
            // that we have one more to get parameters from
            ret_vec.push((add(&prev.0, xorout), prev.1));
            (copy_poly(&xorout), InitPlace::None)
        }
        None => (copy_poly(&prev.0), prev.1),
    };
    for (p, l) in polys.into_iter().rev() {
        let appendix = match (maybe_xorout, l != InitPlace::None && l == prev.1) {
            (None, _) | (_, true) => {
                let poly_diff = add(&p, &prev.0);
                let new_init_place = match (prev.1, l) {
                    // no coefficients being one means it is zero and therefore the neutral element
                    (InitPlace::None, other) | (other, InitPlace::None) => other,
                    (InitPlace::Single(l1), InitPlace::Single(l2)) => {
                        if l1 == l2 {
                            // they cancel out
                            InitPlace::None
                        } else {
                            InitPlace::Pair(l1, l2)
                        }
                    }
                    (InitPlace::Pair(_, _), _) | (_, InitPlace::Pair(_, _)) => {
                        panic!("Internal Error: init pair in the input array of remove_xorouts")
                    }
                };
                (poly_diff, new_init_place)
            }
            (Some(xorout), false) => {
                let poly_no_xorout = add(&p, xorout);
                (poly_no_xorout, l)
            }
        };
        ret_vec.push(appendix);
        prev = (p, l);
    }
    (ret_vec, xor_ret)
}

fn find_polyhull(spec: &RevInfo, polys: Vec<InitPoly>, verbosity: u64) -> (Vec<InitPoly>, PolyPtr) {
    let log = |s| {
        if verbosity > 1 {
            eprintln!(
                "<crc poly, refin = {}, refout = {}> {}",
                spec.refin, spec.refout, s
            );
        }
    };
    let mut contain_init_vec = Vec::new();
    let mut hull = spec
        .poly
        .as_ref()
        .map(|x| copy_poly(x))
        .unwrap_or_else(new_zero);
    log("gcd'ing same length files together");
    for (p, l) in polys {
        match l {
            InitPlace::None => {
                // if init is multiplied by 0, this is already a multiple of poly so we can gcd it to our estimate
                hull.pin_mut().gcd_to(&p);
            }
            _ => {
                contain_init_vec.push((p, l));
            }
        }
        if deg(&hull) == 0 {
            return (contain_init_vec, hull);
        }
    }

    log("gcd'ing different length files together");
    for ((p, l), (q, m)) in contain_init_vec.iter().zip(contain_init_vec.iter().skip(1)) {
        let power_8n = |n: usize| new_poly_shifted(&[1], 8 * n as i64, true);
        // this essentially tries to cancel out the init in the checksums
        // if you have a*init and b*init, you can get 0 by calculating b*a*init - a*b*init
        // this is almost done here, except for cancelling unneccessary common X^k between a and b
        let (mut p_fac, mut q_fac) = match (l, m) {
            (InitPlace::None, _) | (_, InitPlace::None) => unreachable!(),
            (InitPlace::Single(d), InitPlace::Single(e)) => {
                let min = d.min(e);
                (power_8n(d - min), power_8n(e - min))
            }
            (InitPlace::Single(d), InitPlace::Pair(e1, e2)) => {
                let min = d.min(e1).min(e2);
                let p_fac = power_8n(d - min);
                let mut q_fac = power_8n(e2 - min);
                q_fac += &power_8n(e1 - min);
                (p_fac, q_fac)
            }
            (InitPlace::Pair(d1, d2), InitPlace::Single(e)) => {
                let min = d1.min(d1).min(e);
                let mut p_fac = power_8n(d2 - min);
                p_fac += &power_8n(d1 - min);
                let q_fac = power_8n(e - min);
                (p_fac, q_fac)
            }
            (InitPlace::Pair(d1, d2), InitPlace::Pair(e1, e2)) => {
                let min = d1.min(d2).min(e1).min(e2);
                let mut p_fac = power_8n(d2 - min);
                p_fac += &power_8n(d1 - min);
                let mut q_fac = power_8n(e2 - min);
                q_fac += &power_8n(e1 - min);
                (p_fac, q_fac)
            }
        };
        p_fac *= q;
        q_fac *= p;
        q_fac += &p_fac;
        // q_fac should now contain no init, so we can gcd it to the hull
        hull.pin_mut().gcd_to(&q_fac);
        if deg(&hull) == 0 {
            return (contain_init_vec, hull);
        }
    }

    if hull.is_zero() {
        // nothing i can do to help now really
        eprintln!("Error: very unlucky choice of input files, skipping crc reversing");
        return (contain_init_vec, new_poly(&[1]));
    }

    log("removing factors with degree*multiplicity > width");
    // You may remember from your course in abstract algebra that in GF(q)[X],
    // the polynomial p_d = X^(q^d) - X contains all primes with degrees dividing d (with multiplicty 1)
    // Here, we multiply all such polynomials for d from 1 to width together.
    // In this product, each prime of degree k has multiplicity floor(width/k) since there are
    // exactly floor(width/k) p_d where k divides d.
    // Now, that polynomial would be quite large, but we only care about the gcd of this polynomial
    // with hull, so we can evaluated this modulo hull.
    let mut cumulative_prod = new_polyrem(&new_poly(&[1]), &hull);
    let x = new_polyrem(&new_poly(&[1 << 1]), &hull);
    let mut x_to_2_to_n = copy_polyrem(&x);
    for i in 0..spec.width {
        if verbosity > 1 {
            eprintln!(
                "<crc poly, refin = {}, refout = {}> step {} of {}",
                spec.refin, spec.refout, i, spec.width
            )
        }
        x_to_2_to_n.pin_mut().sqr();
        let mut fac = copy_polyrem(&x_to_2_to_n);
        fac += &x;
        // (fac = x^(2^n) + x)
        cumulative_prod *= &fac;
    }
    drop(x_to_2_to_n);
    let reduced_prod = cumulative_prod.rep();
    drop(cumulative_prod);
    log("doing final gcd");
    hull.pin_mut().gcd_to(&reduced_prod);
    log("removing trailing zeros");
    // we don't care about the factor X^k in the hull, since crc polys should
    // have the lowest bit set (why would you not??)
    // it is also assumed later that this holds, so this can not just be removed
    for i in 0..=spec.width {
        if hull.coeff(i as i64) {
            hull = shift(&hull, -(i as i64));
            break;
        }
    }
    (contain_init_vec, hull)
}

// we don't actually ever convert the factors represented by a
// InitPlaces struct into a full polynomial, we just evaluate it modulo the hull
// to do this faster, we save X^k mod hull and evaluate them from smallest to largest
// so we can reuse it later
struct MemoPower {
    prev_power: usize,
    prev_ppoly: PolyRemPtr,
    init_fac: PolyPtr,
    hull: PolyPtr,
}
impl MemoPower {
    fn new(hull: &Poly) -> Self {
        MemoPower {
            prev_power: 0,
            prev_ppoly: new_polyrem(&new_poly(&[1]), &hull),
            init_fac: new_zero(),
            hull: copy_poly(hull),
        }
    }
    fn update_init_fac(&mut self, place: &InitPlace) -> &Poly {
        let mut update_power = |&new_level: &usize| {
            if new_level < self.prev_power {
                panic!("Internal Error: Polynomials non-ascending");
            }
            let x = new_polyrem(&new_poly(&[1 << 1]), &self.hull);
            let power_diff = powermod(&x, (new_level - self.prev_power) as i64 * 8);
            self.prev_power = new_level;
            self.prev_ppoly *= &power_diff;
            self.prev_ppoly.rep()
        };
        self.init_fac = match place {
            InitPlace::None => new_zero(),
            InitPlace::Single(d) => update_power(d),
            InitPlace::Pair(d1, d2) => {
                let mut current_power = update_power(d1);
                current_power += &update_power(d2);
                current_power
            }
        };
        &self.init_fac
    }
    fn get_init_fac(&self) -> &Poly {
        &self.init_fac
    }
    fn update_hull(&mut self, hull: &Poly) {
        self.hull = copy_poly(hull);
        self.prev_ppoly = new_polyrem(&self.prev_ppoly.rep(), &hull)
    }
}
// describes a set of solutions for unknown*possible % hull
struct PrefactorMod {
    unknown: PolyPtr,
    possible: PolyPtr,
    hull: PolyPtr,
}

impl PrefactorMod {
    fn empty() -> Self {
        PrefactorMod {
            unknown: new_poly(&[1]),
            possible: new_zero(),
            hull: new_poly(&[1]),
        }
    }
    fn new_init(maybe_init: &Option<PolyPtr>, hull: &Poly) -> Self {
        // if we already have init, we can use that for our solution here, otherwise use the
        // set of all possible solutions
        let (unknown, possible) = match maybe_init {
            None => (copy_poly(hull), new_zero()),
            Some(init) => (new_poly(&[1]), copy_poly(init)),
        };
        PrefactorMod {
            unknown,
            possible,
            hull: copy_poly(&hull),
        }
    }

    fn new_file(
        mut file: PolyPtr,
        power: &mut MemoPower,
        mut hull: Pin<&mut Poly>,
    ) -> Option<Self> {
        file.pin_mut().rem_to(&hull);
        let file_float = gcd(&file, &hull);
        let power_float = gcd(power.get_init_fac(), &hull);
        let common_float = gcd(&power_float, &file_float);
        // power_float has to divide file_float in the hull
        let discrepancy = div(&power_float, &common_float);
        if !discrepancy.eq(&new_poly(&[1])) {
            // if it does not, we change the hull so that it does
            // by replacing the hull_part with the file_part in the hull
            let hull_part = highest_power_gcd(&hull, &discrepancy);
            let file_part = gcd(&file_float, &hull_part);
            // since discrepancy divides file_part and file_part divides hull, resue file_part here
            hull.as_mut().div_to(&hull_part);
            hull.as_mut().mul_to(&file_part);
            if deg(&hull) <= 0 {
                return None;
            }
            power.update_hull(&hull);
        }
        drop(discrepancy);
        drop(power_float);
        drop(file_float);
        // since we only have power*init ≡ mod hull, but want to calculate init,
        // we need to calculate the modular inverse
        let possible = inverse_fixed(file, power.get_init_fac(), &common_float, &hull);
        Some(PrefactorMod {
            unknown: common_float,
            possible,
            hull: copy_poly(&hull),
        })
    }

    fn update_hull(&mut self, hull: &Poly) {
        if self.hull.eq(hull) {
            return;
        }
        self.hull = copy_poly(hull);
        self.unknown.pin_mut().gcd_to(hull);
        self.possible %= &self.valid();
    }

    // merge two different sets of solutions into one where the hull is the gcd of both
    // and all solutions are valid in both
    fn merge(mut self, mut other: Self, mut hull: Pin<&mut Poly>) -> Option<Self> {
        self.update_hull(&hull);
        other.update_hull(&hull);
        self.adjust_compability(&mut other, hull.as_mut());
        if deg(&hull) <= 0 {
            return None;
        }
        let mut self_fac = new_zero();
        let mut other_fac = new_zero();
        let self_valid = self.valid();
        let other_valid = other.valid();
        // this is the chinese remainder theorem for non-coprime ideals
        let common_valid = xgcd(
            self_fac.pin_mut(),
            other_fac.pin_mut(),
            &self_valid,
            &other_valid,
        );
        self_fac *= &self_valid;
        self_fac *= &other.possible;
        other_fac *= &other_valid;
        other_fac *= &self.possible;
        self_fac += &other_fac;
        self_fac /= &common_valid;
        self.possible = self_fac;
        self.unknown = gcd(&self.unknown, &other.unknown);
        Some(self)
    }

    // in order to chinese remainder with a common factor, both polynomials modulo
    // the common factor need to be the same
    // if this is not the case, the hull is adjusted
    fn adjust_compability(&mut self, other: &mut Self, mut hull: Pin<&mut Poly>) {
        let common_valid = gcd(&self.valid(), &other.valid());
        let actual_valid = gcd(&add(&self.possible, &other.possible), &common_valid);
        hull.as_mut().div_to(&common_valid);
        hull.as_mut().mul_to(&actual_valid);
        if deg(&hull) <= 0 {
            return;
        }
        self.update_hull(&hull);
        other.update_hull(&hull);
    }

    fn valid(&self) -> PolyPtr {
        div(&self.hull, &self.unknown)
    }

    fn iter_inits(
        &self,
        red_poly: &Poly,
        xorout: &InitPoly,
    ) -> impl Iterator<Item = (PolyPtr, PolyPtr, PolyPtr)> {
        let red_unknown = gcd(&self.unknown, red_poly);
        let red_valid = div(red_poly, &red_unknown);
        let red_init = rem(&self.possible, &red_valid);
        let mod_valid = new_polyrem(&red_valid, red_poly);
        let mod_init = new_polyrem(&red_init, red_poly);
        let mod_xorout = new_polyrem(&xorout.0, red_poly);
        let x = new_polyrem(&new_poly(&[&1 << 1]), red_poly);
        let mod_power = match xorout.1 {
            InitPlace::None => new_polyrem(&new_zero(), red_poly),
            InitPlace::Single(l) => powermod(&x, 8 * l as i64),
            _ => panic!("Internal Error: Double"),
        };
        let poly_copy = copy_poly(red_poly);
        // iterate over all polynomials p mod red_unknown and calculate possible + valid*p
        (0u128..1 << deg(&red_unknown)).map(move |p| {
            let mut current_init = new_polyrem(&new_poly(&p.to_be_bytes()), &poly_copy);
            current_init *= &mod_valid;
            current_init += &mod_init;
            // also calculate the corresponding xorouts while we're at it
            let mut current_xorout = copy_polyrem(&mod_power);
            current_xorout *= &current_init;
            current_xorout += &mod_xorout;
            (
                copy_poly(&poly_copy),
                current_init.rep(),
                current_xorout.rep(),
            )
        })
    }
}

fn find_init(
    maybe_init: &Option<PolyPtr>,
    mut hull: Pin<&mut Poly>,
    polys: Vec<InitPoly>,
) -> PrefactorMod {
    if deg(&hull) <= 0 {
        return PrefactorMod::empty();
    }
    let mut ret = PrefactorMod::new_init(maybe_init, &hull);
    let mut power = MemoPower::new(&hull);
    for (p, l) in polys {
        power.update_init_fac(&l);
        let file_solutions = PrefactorMod::new_file(p, &mut power, hull.as_mut());
        ret = match file_solutions
            .map(|f| ret.merge(f, hull.as_mut()))
            .flatten()
        {
            Some(valid) => valid,
            None => return PrefactorMod::empty(),
        }
    }
    ret
}

// calculates lim_{n to inf} gcd(a, b^n)
fn highest_power_gcd(a: &Poly, b: &Poly) -> PolyPtr {
    let mut prev = new_poly(&[1]);
    let mut cur = b % a;
    while !cur.eq(&prev) {
        prev = copy_poly(&cur);
        cur.pin_mut().sqr();
        cur.pin_mut().gcd_to(a);
    }
    cur
}

// ntl's modular division doesn't account for common factors between
// the arguments, so this is a version which does
fn inverse_fixed(mut a: PolyPtr, b: &Poly, common: &Poly, hull: &Poly) -> PolyPtr {
    a /= common;
    let mut b = copy_poly(b);
    b /= common;
    let module = div(hull, common);
    if module.eq(&new_poly(&[1])) {
        return new_zero();
    }
    let mut ma = new_polyrem(&a, &module);
    let mb = new_polyrem(&b, &module);
    ma /= &mb;
    ma.rep()
}

fn find_prod_comb(
    width: usize,
    // (degree, multiplicity)
    gens: &[(PolyPtr, i64)],
) -> Vec<PolyPtr> {
    // there's no reason i implemented it like this in particular; the problem is NP complete
    // and i've got no clue how to efficiently solve it anyway and this seemed like a simple solution
    let mut ret: Vec<Vec<PolyPtr>> = (0..=width).map(|_| Vec::new()).collect();
    for (p, l) in gens.iter() {
        // since Poly doesn't implement clone, this will have to do for now
        let retcopy: Vec<Vec<_>> = ret
            .iter()
            .map(|v| v.iter().map(|q| copy_poly(q)).collect())
            .collect();
        let mut q = copy_poly(p);
        for _ in 1..=*l {
            let inc_deg = deg(&q) as usize;
            if inc_deg > width {
                break;
            }
            ret[inc_deg].push(copy_poly(&q));
            for (j, el) in retcopy[0..=width as usize - inc_deg].iter().enumerate() {
                for m in el {
                    ret[j + inc_deg].push(mul(&q, m));
                }
            }
            q *= &p;
        }
    }
    ret.pop().unwrap()
}

fn bytes_to_poly(bytes: &[u8], checksum: u128, width: u8, refin: bool, refout: bool) -> PolyPtr {
    let mut poly = new_poly_shifted(bytes, width as i64, !refin);
    let check_mask = 1u128.checked_shl(width as u32).unwrap().wrapping_sub(1);
    let check = check_mask & cond_reverse(width, checksum, refout);
    poly += &new_poly(&check.to_be_bytes());
    poly
}

fn cond_reverse(width: u8, value: u128, refout: bool) -> u128 {
    if refout {
        value.reverse_bits() >> (128 - width)
    } else {
        value
    }
}

fn poly_to_u128(poly: &Poly) -> u128 {
    u128::from_be_bytes(
        poly.to_bytes(16)
            .as_ref()
            .unwrap()
            .as_slice()
            .try_into()
            .unwrap(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::checksum::{
        crc::{CRCBuilder, CRC},
        Digest,
    };
    use quickcheck::{Arbitrary, TestResult};
    use std::convert::TryInto;
    impl Arbitrary for CRCBuilder<u128> {
        fn arbitrary<G: quickcheck::Gen>(g: &mut G) -> Self {
            let width = (u8::arbitrary(g) % 128) + 1;
            let poly = (u128::arbitrary(g) % (1 << width)) | 1;
            let init = u128::arbitrary(g) % (1 << width);
            let xorout = u128::arbitrary(g) % (1 << width);
            let refout = bool::arbitrary(g);
            let refin = bool::arbitrary(g);
            let mut builder = CRC::<u128>::with_options();
            builder
                .width(usize::from(width))
                .poly(poly)
                .init(init)
                .xorout(xorout)
                .refout(refout)
                .refin(refin);
            builder
        }
    }
    fn get_polys_from_crc(crc: &CRC<u128>) -> (PolyPtr, PolyPtr, PolyPtr) {
        (
            add(
                &new_poly(&crc.poly.to_be_bytes()),
                &new_poly_shifted(&[1], crc.width as i64, true),
            ),
            new_poly(&crc.init.to_be_bytes()),
            new_poly(&cond_reverse(crc.width as u8, crc.xorout, crc.refout).to_be_bytes()),
        )
    }
    fn prepare_xoroutless(
        files: &mut [Vec<u8>],
        crc_build: &CRCBuilder<u128>,
        remove_xorout: bool,
    ) -> Option<Vec<InitPoly>> {
        let crc = crc_build.build().ok()?;
        files.sort_by(|a, b| a.len().cmp(&b.len()).then(a.cmp(&b)).reverse());
        if files.iter().zip(files.iter().skip(1)).any(|(a, b)| a == b) || files.len() <= 3 {
            return None;
        }
        let (_, _, xorout_p) = get_polys_from_crc(&crc);
        let maybe_xorout = if remove_xorout { Some(xorout_p) } else { None };
        let mut polys = Vec::new();
        for file in files {
            let check = crc.digest(file.as_slice()).unwrap();
            let file_poly = bytes_to_poly(&file, check, crc.width as u8, crc.refin, crc.refout);
            polys.push((file_poly, InitPlace::Single(file.len())));
        }
        let (polys, _) = remove_xorouts(&maybe_xorout, polys);
        Some(polys)
    }
    #[quickcheck]
    fn test_remove_init(files: Vec<Vec<u8>>, crc_build: CRCBuilder<u128>) -> TestResult {
        let crc = match crc_build.build() {
            Ok(c) => c,
            Err(_) => return TestResult::discard(),
        };
        let (poly_p, init_p, _) = get_polys_from_crc(&crc);
        let mut polys = Vec::new();
        for file in files {
            let check = crc.digest(file.as_slice()).unwrap() ^ crc.xorout;
            let file_poly = bytes_to_poly(&file, check, crc.width as u8, crc.refin, crc.refout);
            polys.push((file_poly, InitPlace::Single(file.len())));
        }
        remove_inits(&init_p, &mut polys);
        TestResult::from_bool(polys.iter().all(|p| rem(&p.0, &poly_p).is_zero()))
    }
    #[quickcheck]
    fn test_remove_xorout(
        files: Vec<Vec<u8>>,
        mut crc_build: CRCBuilder<u128>,
        known: bool,
    ) -> TestResult {
        if files.is_empty() {
            return TestResult::discard();
        }
        let do_stuff = |builder: &CRCBuilder<u128>| {
            let crc = builder.build().unwrap();
            let mut polys = Vec::new();
            for file in &files {
                let check = crc.digest(file.as_slice()).unwrap();
                let file_poly = bytes_to_poly(file, check, crc.width as u8, crc.refin, crc.refout);
                polys.push((file_poly, InitPlace::Single(file.len())));
            }
            let maybe_xorout = if known {
                Some(get_polys_from_crc(&crc).2)
            } else {
                None
            };
            let (polys, _) = remove_xorouts(&maybe_xorout, polys);
            polys
        };
        let polys_x = do_stuff(&crc_build);
        crc_build.xorout(0);
        let polys_nox = do_stuff(&crc_build);
        TestResult::from_bool(polys_x.iter().zip(polys_nox).all(|(a, b)| a.0.eq(&b.0)))
    }
    #[quickcheck]
    fn test_poly_hull(
        mut files: Vec<Vec<u8>>,
        crc_build: CRCBuilder<u128>,
        remove_xorout: bool,
    ) -> TestResult {
        if let Some(polys) = prepare_xoroutless(&mut files, &crc_build, remove_xorout) {
            let crc = crc_build.build().unwrap();
            let (poly_p, _, _) = get_polys_from_crc(&crc);
            let (_, hull) = find_polyhull(
                &RevInfo {
                    width: crc_build.width.unwrap(),
                    init: None,
                    xorout: None,
                    poly: None,
                    refin: crc_build.refin.unwrap(),
                    refout: crc_build.refout.unwrap(),
                },
                polys,
                0,
            );
            TestResult::from_bool(rem(&hull, &poly_p).is_zero())
        } else {
            TestResult::discard()
        }
    }
    #[quickcheck]
    fn test_find_init(
        mut files: Vec<Vec<u8>>,
        poly_factor: Vec<u8>,
        crc_build: CRCBuilder<u128>,
        remove_xorout: bool,
    ) -> TestResult {
        if poly_factor.iter().all(|x| *x == 0) {
            return TestResult::discard();
        }
        if let Some(polys) = prepare_xoroutless(&mut files, &crc_build, remove_xorout) {
            let crc = crc_build.build().unwrap();
            let (poly_p, mut init_p, _) = get_polys_from_crc(&crc);
            let mut multiple_poly = mul(&poly_p, &new_poly(&poly_factor));
            let mut init = find_init(&None, multiple_poly.pin_mut(), polys);
            if !rem(&multiple_poly, &poly_p).is_zero() {
                return TestResult::failed();
            }
            init.update_hull(&poly_p);
            init.possible *= &init.unknown;
            init.possible %= &poly_p;
            init_p *= &init.unknown;
            init_p %= &poly_p;
            TestResult::from_bool(init.possible.eq(&init_p))
        } else {
            TestResult::discard()
        }
    }
    #[test]
    fn prodcomb() {
        let mut all = [0; 256];
        let x = new_poly(&[1 << 1]);
        let mut power = copy_poly(&x);
        let mut cumulative = new_poly(&[1]);
        for _ in 1..=8 {
            power *= &copy_poly(&power);
            let power_plus_x = add(&power, &x);
            cumulative *= &power_plus_x;
        }
        let factors: Vec<_> = factor(&cumulative, 0)
            .into_iter()
            .map(|PolyI64Pair { poly, l }| (copy_poly(poly), *l))
            .collect();
        let should_be_all_bytes_from_256_to_511_but_as_polys = find_prod_comb(8, &factors);
        should_be_all_bytes_from_256_to_511_but_as_polys
            .iter()
            .map(|p| {
                usize::from_be_bytes(
                    p.to_bytes(std::mem::size_of::<usize>() as i64)
                        .as_ref()
                        .unwrap()
                        .as_slice()
                        .try_into()
                        .unwrap(),
                )
            })
            .for_each(|x| all[x - 256] += 1);
        assert!(all.iter().all(|x| *x == 1));
    }
    #[quickcheck]
    fn qc_crc_rev(
        mut files: Vec<Vec<u8>>,
        crc_build: CRCBuilder<u128>,
        known: (bool, bool, bool, bool, bool),
    ) -> TestResult {
        files.sort_by(|a, b| a.len().cmp(&b.len()).then(a.cmp(&b)).reverse());
        if files.iter().zip(files.iter().skip(1)).any(|(a, b)| a == b) || files.len() <= 3 {
            return TestResult::discard();
        }
        let crc = crc_build.build().unwrap();
        let mut naive = CRC::<u128>::with_options();
        naive.width(crc_build.width.unwrap());
        if known.0 {
            naive.poly(crc_build.poly.unwrap());
        }
        if known.1 {
            naive.init(crc_build.init.unwrap());
        }
        if known.2 {
            naive.xorout(crc_build.xorout.unwrap());
        }
        if known.3 {
            naive.refin(crc_build.refin.unwrap());
        }
        if known.4 {
            naive.refout(crc_build.refout.unwrap());
        }
        let chk_files: Vec<_> = files
            .iter()
            .map(|f| {
                let checksum = crc.digest(f.as_slice()).unwrap();
                (f.as_slice(), checksum)
            })
            .collect();
        let reverser = reverse_crc(&naive, &chk_files, 0);
        let mut has_appeared = false;
        for crc_loop in reverser {
            let crc_loop = match crc_loop {
                Ok(x) => x,
                Err(_) => return TestResult::discard(),
            };
            if !has_appeared && crc_loop == crc {
                has_appeared = true;
            }
            for (file, original_check) in &chk_files {
                let checksum = crc_loop.digest(*file).unwrap();
                if checksum != *original_check {
                    eprintln!("expected checksum: {:x}", original_check);
                    eprintln!("actual checksum: {:x}", checksum);
                    eprintln!("crc: {}", crc_loop);
                    return TestResult::failed();
                }
            }
        }
        TestResult::from_bool(has_appeared)
    }
    #[test]
    fn test_crc32() {
        let crc = CRC::<u32>::with_options()
            .poly(0x04c11db7)
            .width(32)
            .init(0xffffffff)
            .xorout(0xffffffff)
            .refout(true)
            .refin(true)
            .build()
            .unwrap();
        let files = vec![
            vec![0x12u8, 0x34u8, 0x56u8],
            vec![0x67u8, 0x41u8, 0xffu8],
            vec![0x15u8, 0x56u8, 0x76u8, 0x1fu8],
            vec![0x14u8, 0x62u8, 0x51u8, 0xa4u8, 0xd3u8],
        ];
        let chk_files: Vec<_> = files
            .iter()
            .map(|f| {
                let checksum = crc.digest(f.as_slice()).unwrap().into();
                println!("{:?} {:x}", &f, checksum);
                (f.as_slice(), checksum)
            })
            .collect();
        let mut crc_naive = CRC::<u128>::with_options();
        crc_naive.width(32).refin(true).refout(true);
        for c in reverse_crc(&crc_naive, &chk_files, 0) {
            let n = c.unwrap();
            for (file, original_check) in &chk_files {
                let checksum = n.digest(*file).unwrap();
                assert_eq!(checksum, *original_check);
            }
        }
    }
    #[test]
    fn test_crc16() {
        let crc = CRC::<u128>::with_options()
            .poly(0x8005)
            .width(16)
            .refin(true)
            .refout(true)
            .build()
            .unwrap();
        let files = vec![
            vec![0x12u8, 0x34u8, 0x56u8],
            vec![0x67u8, 0x41u8, 0xffu8],
            vec![0x15u8, 0x56u8, 0x76u8, 0x1fu8],
            vec![0x14u8, 0x62u8, 0x51u8, 0xa4u8, 0xd3u8],
        ];
        let chk_files: Vec<_> = files
            .iter()
            .map(|f| {
                let checksum = crc.digest(f.as_slice()).unwrap();
                println!("{:?} {:x}", &f, checksum);
                (f.as_slice(), checksum)
            })
            .collect();
        let mut crc_naive = CRC::<u128>::with_options();
        crc_naive.width(16).refin(true).refout(true);
        for c in reverse_crc(&crc_naive, &chk_files, 0) {
            let n = c.unwrap();
            for (file, original_check) in &chk_files {
                let checksum = n.digest(*file).unwrap();
                assert_eq!(checksum, *original_check);
            }
        }
    }
}
