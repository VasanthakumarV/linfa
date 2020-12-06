use super::permutable_kernel::Permutable;
use super::{ExitReason, Float, Svm};

use ndarray::{Array1, Axis};
use std::marker::PhantomData;

/// Parameters of the solver routine
#[derive(Clone)]
pub struct SolverParams<A: Float> {
    /// Stopping condition
    pub eps: A,
    /// Should we shrink, e.g. ignore bounded alphas
    pub shrinking: bool,
}

/// Status of alpha variables of the solver
#[derive(Debug)]
struct Alpha<A: Float> {
    value: A,
    upper_bound: A,
}

impl<A: Float> Alpha<A> {
    pub fn from(value: A, upper_bound: A) -> Alpha<A> {
        Alpha { value, upper_bound }
    }

    pub fn reached_upper(&self) -> bool {
        self.value >= self.upper_bound
    }

    pub fn free_floating(&self) -> bool {
        self.value < self.upper_bound && self.value > A::zero()
    }

    pub fn reached_lower(&self) -> bool {
        self.value == A::zero()
    }

    pub fn val(&self) -> A {
        self.value
    }
}

/// Current state of the SMO solver
///
/// We are solving the dual problem with linear constraints
/// min_a f(a), s.t. y^Ta = d, 0 <= a_t < C, t = 1, ..., l
/// where f(a) = a^T Q a / 2 + p^T a
pub struct SolverState<'a, A: Float, K: Permutable<'a, A>> {
    /// Gradient of each variable
    gradient: Vec<A>,
    /// Cached gradient because most of the variables are constant
    gradient_fixed: Vec<A>,
    /// Current value of each variable and in respect to bounds
    alpha: Vec<Alpha<A>>,
    /// Active set of variables
    active_set: Vec<usize>,
    /// Number of active variables
    nactive: usize,
    unshrink: bool,
    nu_constraint: bool,
    r: A,

    /// Quadratic term of the problem
    kernel: K,
    /// Linear term of the problem
    p: Vec<A>,
    /// Targets we want to predict
    targets: Vec<bool>,
    /// Bounds per alpha
    bounds: Vec<A>,

    /// Parameters, e.g. stopping condition etc.
    params: SolverParams<A>,

    phantom: PhantomData<&'a K>,
}

#[allow(clippy::needless_range_loop)]
impl<'a, A: Float, K: 'a + Permutable<'a, A>> SolverState<'a, A, K> {
    /// Initialize a solver state
    ///
    /// This is bounded by the lifetime of the kernel matrix, because it can quite large
    pub fn new(
        alpha: Vec<A>,
        p: Vec<A>,
        targets: Vec<bool>,
        kernel: K,
        bounds: Vec<A>,
        params: SolverParams<A>,
        nu_constraint: bool,
    ) -> SolverState<'a, A, K> {
        // initialize alpha status according to bound
        let alpha = alpha
            .into_iter()
            .enumerate()
            .map(|(i, alpha)| Alpha::from(alpha, bounds[i]))
            .collect::<Vec<_>>();

        // initialize full active set
        let active_set = (0..alpha.len()).map(|i| i).collect::<Vec<_>>();

        // initialize gradient
        let mut gradient = p.clone();
        let mut gradient_fixed = vec![A::zero(); alpha.len()];

        for i in 0..alpha.len() {
            // when we have reached alpha = A::zero(), then d(a) = p
            if !alpha[i].reached_lower() {
                let dist_i = kernel.distances(i, alpha.len());
                let alpha_i = alpha[i].val();

                // update gradient as d(a) = p + Q a
                for j in 0..alpha.len() {
                    gradient[j] += alpha_i * dist_i[j];
                }

                // Cache gradient when we reached the upper bound for a variable
                if alpha[i].reached_upper() {
                    for j in 0..alpha.len() {
                        gradient_fixed[j] += bounds[i] * dist_i[j];
                    }
                }
            }
        }

        SolverState {
            gradient,
            gradient_fixed,
            alpha,
            p,
            nactive: active_set.len(),
            unshrink: false,
            active_set,
            kernel,
            targets,
            bounds,
            params,
            nu_constraint,
            r: A::zero(),
            phantom: PhantomData,
        }
    }

    /// Return number of active variables
    pub fn nactive(&self) -> usize {
        self.nactive
    }

    /// Return number of total variables
    pub fn ntotal(&self) -> usize {
        self.alpha.len()
    }

    /// Return target as positive/negative indicator
    pub fn target(&self, idx: usize) -> A {
        if self.targets[idx] {
            A::one()
        } else {
            -A::one()
        }
    }

    /// Return the k-th bound
    pub fn bound(&self, idx: usize) -> A {
        self.bounds[idx]
    }

    /// Swap two variables
    pub fn swap(&mut self, i: usize, j: usize) {
        self.gradient.swap(i, j);
        self.gradient_fixed.swap(i, j);
        self.alpha.swap(i, j);
        self.p.swap(i, j);
        self.active_set.swap(i, j);
        self.kernel.swap_indices(i, j);
        self.targets.swap(i, j);
    }

    /// Reconstruct gradients from inactivate variables
    ///
    /// A variables is inactive, when it reaches the upper bound.
    ///
    fn reconstruct_gradient(&mut self) {
        // if no variable is inactive, skip
        if self.nactive() == self.ntotal() {
            return;
        }

        // d(a_i) = G^_i + p_i + ...
        for j in self.nactive()..self.ntotal() {
            self.gradient[j] = self.gradient_fixed[j] + self.p[j];
        }

        let nfree: usize = (0..self.nactive())
            .filter(|x| self.alpha[*x].free_floating())
            .count();
        if nfree * self.ntotal() > 2 * self.nactive() * (self.ntotal() - self.nactive()) {
            for i in self.nactive()..self.ntotal() {
                let dist_i = self.kernel.distances(i, self.nactive());
                for j in 0..self.nactive() {
                    if self.alpha[i].free_floating() {
                        self.gradient[i] += self.alpha[j].val() * dist_i[j];
                    }
                }
            }
        } else {
            for i in 0..self.nactive() {
                if self.alpha[i].free_floating() {
                    let dist_i = self.kernel.distances(i, self.ntotal());
                    let alpha_i = self.alpha[i].val();
                    for j in self.nactive()..self.ntotal() {
                        self.gradient[j] += alpha_i * dist_i[j];
                    }
                }
            }
        }
    }

    pub fn update(&mut self, working_set: (usize, usize)) {
        // working set indices are called i, j here
        let (i, j) = working_set;

        let dist_i = self.kernel.distances(i, self.nactive());
        let dist_j = self.kernel.distances(j, self.nactive());

        let bound_i = self.bound(i);
        let bound_j = self.bound(j);

        let old_alpha_i = self.alpha[i].val();
        let old_alpha_j = self.alpha[j].val();

        if self.targets[i] != self.targets[j] {
            let mut quad_coef = self.kernel.self_distance(i)
                + self.kernel.self_distance(j)
                + (A::one() + A::one()) * dist_i[j];
            if quad_coef <= A::zero() {
                quad_coef = A::from(1e-10).unwrap();
            }

            let delta = -(self.gradient[i] + self.gradient[j]) / quad_coef;
            let diff = self.alpha[i].val() - self.alpha[j].val();

            // update parameters
            self.alpha[i].value += delta;
            self.alpha[j].value += delta;

            // bound to feasible solution
            if diff > A::zero() {
                if self.alpha[j].val() < A::zero() {
                    self.alpha[j].value = A::zero();
                    self.alpha[i].value = diff;
                }
            } else if self.alpha[i].val() < A::zero() {
                self.alpha[i].value = A::zero();
                self.alpha[j].value = -diff;
            }

            if diff > bound_i - bound_j {
                if self.alpha[i].val() > bound_i {
                    self.alpha[i].value = bound_i;
                    self.alpha[j].value = bound_i - diff;
                }
            } else if self.alpha[j].val() > bound_j {
                self.alpha[j].value = bound_j;
                self.alpha[i].value = bound_j + diff;
            }
        } else {
            //dbg!(self.kernel.self_distance(i), self.kernel.self_distance(j), A::from(2.0).unwrap() * dist_i[j]);
            let mut quad_coef = self.kernel.self_distance(i) + self.kernel.self_distance(j)
                - A::from(2.0).unwrap() * dist_i[j];
            if quad_coef <= A::zero() {
                quad_coef = A::from(1e-10).unwrap();
            }

            let delta = (self.gradient[i] - self.gradient[j]) / quad_coef;
            let sum = self.alpha[i].val() + self.alpha[j].val();

            // update parameters
            self.alpha[i].value -= delta;
            self.alpha[j].value += delta;

            // bound to feasible solution
            if sum > bound_i {
                if self.alpha[i].val() > bound_i {
                    self.alpha[i].value = bound_i;
                    self.alpha[j].value = sum - bound_i;
                }
            } else if self.alpha[j].val() < A::zero() {
                self.alpha[j].value = A::zero();
                self.alpha[i].value = sum;
            }
            if sum > bound_j {
                if self.alpha[j].val() > bound_j {
                    self.alpha[j].value = bound_j;
                    self.alpha[i].value = sum - bound_j;
                }
            } else if self.alpha[i].val() < A::zero() {
                self.alpha[i].value = A::zero();
                self.alpha[j].value = sum;
            }
            /*if self.alpha[i].val() > bound_i {
                self.alpha[i].value = bound_i;
            } else if self.alpha[i].val() < A::zero() {
                self.alpha[i].value = A::zero();
            }

            if self.alpha[j].val() > bound_j {
                self.alpha[j].value = bound_j;
            } else if self.alpha[j].val() < A::zero() {
                self.alpha[j].value = A::zero();
            }*/
        }

        // update gradient
        let delta_alpha_i = self.alpha[i].val() - old_alpha_i;
        let delta_alpha_j = self.alpha[j].val() - old_alpha_j;

        for k in 0..self.nactive() {
            self.gradient[k] += dist_i[k] * delta_alpha_i + dist_j[k] * delta_alpha_j;
        }

        // update alpha status and gradient bar
        let ui = self.alpha[i].reached_upper();
        let uj = self.alpha[j].reached_upper();

        self.alpha[i] = Alpha::from(self.alpha[i].val(), self.bound(i));
        self.alpha[j] = Alpha::from(self.alpha[j].val(), self.bound(j));

        // update gradient of non-free variables if `i` became free or non-free
        if ui != self.alpha[i].reached_upper() {
            let dist_i = self.kernel.distances(i, self.ntotal());
            let bound_i = self.bound(i);
            if ui {
                for k in 0..self.ntotal() {
                    self.gradient_fixed[k] -= bound_i * dist_i[k];
                }
            } else {
                for k in 0..self.ntotal() {
                    self.gradient_fixed[k] += bound_i * dist_i[k];
                }
            }
        }

        // update gradient of non-free variables if `j` became free or non-free
        if uj != self.alpha[j].reached_upper() {
            let dist_j = self.kernel.distances(j, self.ntotal());
            let bound_j = self.bound(j);
            if uj {
                for k in 0..self.nactive() {
                    self.gradient_fixed[k] -= bound_j * dist_j[k];
                }
            } else {
                for k in 0..self.nactive() {
                    self.gradient_fixed[k] += bound_j * dist_j[k];
                }
            }
        }
    }

    /// Return max and min gradients of free variables
    pub fn max_violating_pair(&self) -> ((A, isize), (A, isize)) {
        // max { -y_i * grad(f)_i \i in I_up(\alpha) }
        let mut gmax1 = (-A::infinity(), -1);
        // max { y_i * grad(f)_i \i in U_low(\alpha) }
        let mut gmax2 = (-A::infinity(), -1);

        for i in 0..self.nactive() {
            if self.targets[i] {
                if !self.alpha[i].reached_upper() && -self.gradient[i] >= gmax1.0 {
                    gmax1 = (-self.gradient[i], i as isize);
                }
                if !self.alpha[i].reached_lower() && self.gradient[i] >= gmax2.0 {
                    gmax2 = (self.gradient[i], i as isize);
                }
            } else {
                if !self.alpha[i].reached_upper() && -self.gradient[i] >= gmax2.0 {
                    gmax2 = (-self.gradient[i], i as isize);
                }
                if !self.alpha[i].reached_lower() && self.gradient[i] >= gmax1.0 {
                    gmax1 = (self.gradient[i], i as isize);
                }
            }
        }

        (gmax1, gmax2)
    }

    #[allow(clippy::type_complexity)]
    pub fn max_violating_pair_nu(&self) -> ((A, isize), (A, isize), (A, isize), (A, isize)) {
        let mut gmax1 = (-A::infinity(), -1);
        let mut gmax2 = (-A::infinity(), -1);
        let mut gmax3 = (-A::infinity(), -1);
        let mut gmax4 = (-A::infinity(), -1);

        for i in 0..self.nactive() {
            if self.targets[i] {
                if !self.alpha[i].reached_upper() && -self.gradient[i] > gmax1.0 {
                    gmax1 = (-self.gradient[i], i as isize);
                }
                if !self.alpha[i].reached_lower() && self.gradient[i] > gmax3.0 {
                    gmax3 = (self.gradient[i], i as isize);
                }
            } else {
                if !self.alpha[i].reached_upper() && -self.gradient[i] > gmax4.0 {
                    gmax4 = (-self.gradient[i], i as isize);
                }
                if !self.alpha[i].reached_lower() && self.gradient[i] > gmax2.0 {
                    gmax2 = (self.gradient[i], i as isize);
                }
            }
        }

        (gmax1, gmax2, gmax3, gmax4)
    }

    /// Select optimal working set
    ///
    /// In each optimization step two variables are selected and then optimized. The indices are
    /// selected such that:
    ///  * i: maximizes -y_i * grad(f)_i, i in I_up(\alpha)
    ///  * j: minimizes the decrease of the objective value
    pub fn select_working_set(&self) -> (usize, usize, bool) {
        if self.nu_constraint {
            return self.select_working_set_nu();
        }

        let (gmax, gmax2) = self.max_violating_pair();

        let mut obj_diff_min = (A::infinity(), -1);

        if gmax.1 != -1 {
            let dist_i = self.kernel.distances(gmax.1 as usize, self.ntotal());

            for (j, dist_ij) in dist_i.into_iter().enumerate().take(self.nactive()) {
                if self.targets[j] {
                    if !self.alpha[j].reached_lower() {
                        let grad_diff = gmax.0 + self.gradient[j];
                        if grad_diff > A::zero() {
                            // this is possible, because op_i is some
                            let i = gmax.1 as usize;

                            let quad_coef = self.kernel.self_distance(i)
                                + self.kernel.self_distance(j)
                                - A::from(2.0).unwrap() * self.target(i) * dist_ij;

                            let obj_diff = if quad_coef > A::zero() {
                                -(grad_diff * grad_diff) / quad_coef
                            } else {
                                -(grad_diff * grad_diff) / A::from(1e-10).unwrap()
                            };

                            if obj_diff <= obj_diff_min.0 {
                                obj_diff_min = (obj_diff, j as isize);
                            }
                        }
                    }
                } else if !self.alpha[j].reached_upper() {
                    let grad_diff = gmax.0 - self.gradient[j];
                    if grad_diff > A::zero() {
                        // this is possible, because op_i is `Some`
                        let i = gmax.1 as usize;

                        let quad_coef = self.kernel.self_distance(i)
                            + self.kernel.self_distance(j)
                            + A::from(2.0).unwrap() * self.target(i) * dist_ij;

                        let obj_diff = if quad_coef > A::zero() {
                            -(grad_diff * grad_diff) / quad_coef
                        } else {
                            -(grad_diff * grad_diff) / A::from(1e-10).unwrap()
                        };
                        if obj_diff <= obj_diff_min.0 {
                            obj_diff_min = (obj_diff, j as isize);
                        }
                    }
                }
            }
        }

        if gmax.0 + gmax2.0 < self.params.eps || obj_diff_min.1 == -1 {
            (0, 0, true)
        } else {
            (gmax.1 as usize, obj_diff_min.1 as usize, false)
        }
    }

    /// Select optimal working set
    ///
    /// In each optimization step two variables are selected and then optimized. The indices are
    /// selected such that:
    ///  * i: maximizes -y_i * grad(f)_i, i in I_up(\alpha)
    ///  * j: minimizes the decrease of the objective value
    pub fn select_working_set_nu(&self) -> (usize, usize, bool) {
        let (gmaxp1, gmaxn1, gmaxp2, gmaxn2) = self.max_violating_pair_nu();

        let mut obj_diff_min = (A::infinity(), -1);

        let dist_i_p = if gmaxp1.1 != -1 {
            Some(self.kernel.distances(gmaxp1.1 as usize, self.ntotal()))
        } else {
            None
        };

        let dist_i_n = if gmaxn1.1 != -1 {
            Some(self.kernel.distances(gmaxn1.1 as usize, self.ntotal()))
        } else {
            None
        };

        for j in 0..self.nactive() {
            if self.targets[j] {
                if !self.alpha[j].reached_lower() {
                    let grad_diff = gmaxp1.0 + self.gradient[j];
                    if grad_diff > A::zero() {
                        let dist_i_p = match dist_i_p {
                            Some(ref x) => x,
                            None => continue,
                        };

                        // this is possible, because op_i is some
                        let i = gmaxp1.1 as usize;

                        let quad_coef = self.kernel.self_distance(i) + self.kernel.self_distance(j)
                            - A::from(2.0).unwrap() * dist_i_p[j];

                        let obj_diff = if quad_coef > A::zero() {
                            -(grad_diff * grad_diff) / quad_coef
                        } else {
                            -(grad_diff * grad_diff) / A::from(1e-10).unwrap()
                        };

                        if obj_diff <= obj_diff_min.0 {
                            obj_diff_min = (obj_diff, j as isize);
                        }
                    }
                }
            } else if !self.alpha[j].reached_upper() {
                let grad_diff = gmaxn1.0 - self.gradient[j];
                if grad_diff > A::zero() {
                    let dist_i_n = match dist_i_n {
                        Some(ref x) => x,
                        None => continue,
                    };

                    // this is possible, because op_i is `Some`
                    let i = gmaxn1.1 as usize;

                    let quad_coef = self.kernel.self_distance(i) + self.kernel.self_distance(j)
                        - A::from(2.0).unwrap() * dist_i_n[j];

                    let obj_diff = if quad_coef > A::zero() {
                        -(grad_diff * grad_diff) / quad_coef
                    } else {
                        -(grad_diff * grad_diff) / A::from(1e-10).unwrap()
                    };
                    if obj_diff <= obj_diff_min.0 {
                        obj_diff_min = (obj_diff, j as isize);
                    }
                }
            }
        }

        if A::max(gmaxp1.0 + gmaxp2.0, gmaxn1.0 + gmaxn2.0) < self.params.eps
            || obj_diff_min.1 == -1
        {
            (0, 0, true)
        } else {
            let out_j = obj_diff_min.1 as usize;
            let out_i = if self.targets[out_j] {
                gmaxp1.1 as usize
            } else {
                gmaxn1.1 as usize
            };

            (out_i, out_j, false)
        }
    }

    pub fn should_shrunk(&self, i: usize, gmax1: A, gmax2: A) -> bool {
        if self.alpha[i].reached_upper() {
            if self.targets[i] {
                -self.gradient[i] > gmax1
            } else {
                -self.gradient[i] > gmax2
            }
        } else if self.alpha[i].reached_lower() {
            if self.targets[i] {
                self.gradient[i] > gmax2
            } else {
                -self.gradient[i] > gmax1
            }
        } else {
            false
        }
    }

    pub fn should_shrunk_nu(&self, i: usize, gmax1: A, gmax2: A, gmax3: A, gmax4: A) -> bool {
        if self.alpha[i].reached_upper() {
            if self.targets[i] {
                -self.gradient[i] > gmax1
            } else {
                -self.gradient[i] > gmax4
            }
        } else if self.alpha[i].reached_lower() {
            if self.targets[i] {
                self.gradient[i] > gmax2
            } else {
                self.gradient[i] > gmax3
            }
        } else {
            false
        }
    }

    pub fn do_shrinking(&mut self) {
        if self.nu_constraint {
            self.do_shrinking_nu();
            return;
        }

        let (gmax1, gmax2) = self.max_violating_pair();
        let (gmax1, gmax2) = (gmax1.0, gmax2.0);

        // work on all variables when 10*eps is reached
        if !self.unshrink && gmax1 + gmax2 <= self.params.eps * A::from(10.0).unwrap() {
            self.unshrink = true;
            self.reconstruct_gradient();
            self.nactive = self.ntotal();
        }

        // swap items until working set is homogeneous
        for i in 0..self.nactive() {
            if self.should_shrunk(i, gmax1, gmax2) {
                self.nactive -= 1;
                // only consider items behing this one
                while self.nactive > i {
                    if !self.should_shrunk(self.nactive(), gmax1, gmax2) {
                        self.swap(i, self.nactive());
                        break;
                    }
                    self.nactive -= 1;
                }
            }
        }
    }

    pub fn do_shrinking_nu(&mut self) {
        let (gmax1, gmax2, gmax3, gmax4) = self.max_violating_pair_nu();
        let (gmax1, gmax2, gmax3, gmax4) = (gmax1.0, gmax2.0, gmax3.0, gmax4.0);

        // work on all variables when 10*eps is reached
        if !self.unshrink
            && A::max(gmax1 + gmax2, gmax3 + gmax4) <= self.params.eps * A::from(10.0).unwrap()
        {
            self.unshrink = true;
            self.reconstruct_gradient();
            self.nactive = self.ntotal();
        }

        // swap items until working set is homogeneous
        for i in 0..self.nactive() {
            if self.should_shrunk_nu(i, gmax1, gmax2, gmax3, gmax4) {
                self.nactive -= 1;
                // only consider items behing this one
                while self.nactive > i {
                    if !self.should_shrunk_nu(self.nactive(), gmax1, gmax2, gmax3, gmax4) {
                        self.swap(i, self.nactive());
                        break;
                    }
                    self.nactive -= 1;
                }
            }
        }
    }

    pub fn calculate_rho(&mut self) -> A {
        // with additional constraint call the other function
        if self.nu_constraint {
            return self.calculate_rho_nu();
        }

        let mut nfree = 0;
        let mut sum_free = A::zero();
        let mut ub = A::infinity();
        let mut lb = -A::infinity();

        for i in 0..self.nactive() {
            let yg = self.target(i) * self.gradient[i];

            if self.alpha[i].reached_upper() {
                if self.targets[i] {
                    lb = A::max(lb, yg);
                } else {
                    ub = A::min(ub, yg);
                }
            } else if self.alpha[i].reached_lower() {
                if self.targets[i] {
                    ub = A::min(ub, yg);
                } else {
                    lb = A::max(lb, yg);
                }
            } else {
                nfree += 1;
                sum_free += yg;
            }
        }

        if nfree > 0 {
            sum_free / A::from(nfree).unwrap()
        } else {
            (ub + lb) / A::from(2.0).unwrap()
        }
    }

    pub fn calculate_rho_nu(&mut self) -> A {
        let (mut nfree1, mut nfree2) = (0, 0);
        let (mut sum_free1, mut sum_free2) = (A::zero(), A::zero());
        let (mut ub1, mut ub2) = (A::infinity(), A::infinity());
        let (mut lb1, mut lb2) = (-A::infinity(), -A::infinity());

        for i in 0..self.nactive() {
            if self.targets[i] {
                if self.alpha[i].reached_upper() {
                    lb1 = A::max(lb1, self.gradient[i]);
                } else if self.alpha[i].reached_lower() {
                    ub1 = A::max(ub1, self.gradient[i]);
                } else {
                    nfree1 += 1;
                    sum_free1 += self.gradient[i];
                }
            }

            if !self.targets[i] {
                if self.alpha[i].reached_upper() {
                    lb2 = A::max(lb2, self.gradient[i]);
                } else if self.alpha[i].reached_lower() {
                    ub2 = A::max(ub2, self.gradient[i]);
                } else {
                    nfree2 += 1;
                    sum_free2 += self.gradient[i];
                }
            }
        }

        let r1 = if nfree1 > 0 {
            sum_free1 / A::from(nfree1).unwrap()
        } else {
            (ub1 + lb1) / A::from(2.0).unwrap()
        };
        let r2 = if nfree2 > 0 {
            sum_free2 / A::from(nfree2).unwrap()
        } else {
            (ub2 + lb2) / A::from(2.0).unwrap()
        };

        self.r = (r1 + r2) / A::from(2.0).unwrap();

        (r1 - r2) / A::from(2.0).unwrap()
    }

    pub fn solve(mut self) -> Svm<'a, A, A> {
        let mut iter = 0;
        let max_iter = if self.targets.len() > std::usize::MAX / 100 {
            std::usize::MAX
        } else {
            100 * self.targets.len()
        };

        let max_iter = usize::max(10_000_000, max_iter);
        let mut counter = usize::min(self.targets.len(), 1000) + 1;
        while iter < max_iter {
            counter -= 1;
            if counter == 0 {
                counter = usize::min(self.ntotal(), 1000);
                if self.params.shrinking {
                    self.do_shrinking();
                }
            }

            let (mut i, mut j, is_optimal) = self.select_working_set();
            if is_optimal {
                self.reconstruct_gradient();
                let (i2, j2, is_optimal) = self.select_working_set();
                if is_optimal {
                    break;
                } else {
                    // do shrinking next iteration
                    counter = 1;
                    i = i2;
                    j = j2;
                }
            }

            iter += 1;

            // update alpha[i] and alpha[j]
            self.update((i, j));
        }

        if iter >= max_iter && self.nactive() < self.targets.len() {
            self.reconstruct_gradient();
            self.nactive = self.ntotal();
        }

        let rho = self.calculate_rho();
        let r = if self.nu_constraint {
            Some(self.r)
        } else {
            None
        };

        // calculate object function
        let mut v = A::zero();
        for i in 0..self.targets.len() {
            v += self.alpha[i].val() * (self.gradient[i] + self.p[i]);
        }
        let obj = v / A::from(2.0).unwrap();

        let exit_reason = if max_iter == iter {
            ExitReason::ReachedIterations
        } else {
            ExitReason::ReachedThreshold
        };

        // put back the solution
        let alpha: Vec<A> = (0..self.ntotal())
            .map(|i| self.alpha[self.active_set[i]].val())
            .collect();

        // if the kernel is linear, then we can pre-calculate the dot product
        let linear_decision = if self.kernel.inner().is_linear() {
            let mut tmp = Array1::zeros(self.kernel.inner().dataset.len_of(Axis(1)));
            for (i, elm) in self.kernel.inner().dataset.outer_iter().enumerate() {
                tmp.scaled_add(self.target(i) * alpha[i], &elm);
            }

            Some(tmp)
        } else {
            None
        };

        Svm {
            alpha,
            rho,
            r,
            exit_reason,
            obj,
            iterations: iter,
            kernel: self.kernel.inner(),
            linear_decision,
            phantom: PhantomData,
        }
    }
}

/*
#[cfg(test)]
mod tests {
    use crate::permutable_kernel::PermutableKernel;
    use super::{SolverState, SolverParams, Svm};
    use ndarray::array;
    use linfa_kernel::{Kernel, KernelInner};

    /// Optimize the booth function
    #[test]
    fn test_booth_function() {
        let kernel = array![[10., 8.], [8., 10.]];
        let kernel = Kernel {
            inner: KernelInner::Dense(kernel.clone()),
            fnc: Box::new(|_,_| 0.0),
            dataset: &kernel
        };
        let targets = vec![true, true];
        let kernel = PermutableKernel::new(&kernel, targets.clone());

        let p = vec![-34., -38.];
        let params = SolverParams {
            eps: 1e-6,
            shrinking: false
        };

        let solver = SolverState::new(vec![1.0, 1.0], p, targets, kernel, vec![1000.0; 2], &params, false);

        let res: Svm<f64> = solver.solve();

        println!("{:?}", res.alpha);
        println!("{}", res);


    }
}*/
