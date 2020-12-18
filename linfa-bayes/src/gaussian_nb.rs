//! Gaussian Naive Bayes (GaussianNB)
//!
//! Implements Gaussian Naive Bayes algorithm for classification. The likelihood
//! of the feature P(x_i | y) is assumed to be Gaussian, the mean and variance will
//! be estimated using maximum likelihood.

use ndarray::{s, Array1, Array2, ArrayView1, ArrayView2, Axis};
use ndarray_stats::QuantileExt;
use std::collections::HashMap;

use crate::error::Result;
use linfa::dataset::{Dataset, Labels};
use linfa::traits::{Fit, IncrementalFit, Predict};
use linfa::Float;

/// Gaussian Naive Bayes (GaussianNB)
#[derive(Debug)]
pub struct GaussianNbParams {
    // Required for calculation stability
    var_smoothing: f64,
}

impl Default for GaussianNbParams {
    fn default() -> Self {
        Self::params()
    }
}

impl GaussianNbParams {
    /// Create new GaussianNB model with default values for its parameters
    pub fn params() -> Self {
        GaussianNbParams {
            var_smoothing: 1e-9,
        }
    }

    // Specifies the portion of the largest variance of all the features that
    // is added to the variance for calculation stability
    pub fn var_smoothing(mut self, var_smoothing: f64) -> Self {
        self.var_smoothing = var_smoothing;
        self
    }
}

impl<'a, A, L> Fit<'a, ArrayView2<'_, A>, L> for GaussianNbParams
where
    A: Float,
    L: Labels<Elem = usize>,
{
    type Object = Result<GaussianNb<A>>;

    /// Fit the model
    ///
    /// # Example
    ///
    /// ```no_run
    /// # use ndarray::array;
    /// # use linfa::Dataset;
    /// # use linfa_bayes::GaussianNbParams;
    /// # use linfa::traits::{Fit, Predict};
    /// # use std::error::Error;
    /// # fn main() -> Result<(), Box<dyn Error>> {
    /// let x = array![
    ///     [-2., -1.],
    ///     [-1., -1.],
    ///     [-1., -2.],
    ///     [1., 1.],
    ///     [1., 2.],
    ///     [2., 1.]
    /// ];
    /// let y = vec![1, 1, 1, 2, 2, 2];
    ///
    /// let data = Dataset::new(x.view(), &y);
    /// let model = GaussianNbParams::params().fit(&data)?;
    /// let pred = model.predict(x.view());
    ///
    /// assert_eq!(pred.to_vec(), y);
    /// # Ok(())
    /// # }
    /// ```
    fn fit(&self, dataset: &'a Dataset<ArrayView2<A>, L>) -> Self::Object {
        // We extract the unique classes in sorted order
        let mut unique_classes = dataset.targets.labels();
        unique_classes.sort_unstable();

        // We train the model
        self.fit_with(None, dataset)
    }
}

impl<A, L, I> IncrementalFit<'_, ArrayView2<'_, A>, L, I> for GaussianNbParams
where
    A: Float,
    L: Labels<Elem = usize>,
    I: Into<Option<Result<GaussianNb<A>>>>,
{
    type Object = Result<GaussianNb<A>>;

    /// Incrementally fit on a batch of samples
    ///
    /// # Example
    ///
    /// ```no_run
    /// # use ndarray::{array, Axis};
    /// # use linfa::Dataset;
    /// # use linfa_bayes::GaussianNbParams;
    /// # use linfa::traits::{Predict, IncrementalFit};
    /// # use std::error::Error;
    /// # fn main() -> Result<(), Box<dyn Error>> {
    /// let x = array![
    ///     [-2., -1.],
    ///     [-1., -1.],
    ///     [-1., -2.],
    ///     [1., 1.],
    ///     [1., 2.],
    ///     [2., 1.]
    /// ];
    /// let y = array![1, 1, 1, 2, 2, 2];
    ///
    /// let mut clf = GaussianNbParams::params();
    ///
    /// let model = x.axis_chunks_iter(Axis(0), 2)
    ///    .zip(y.axis_chunks_iter(Axis(0), 2))
    ///    .map(|(a, b)| Dataset::new(a, b))
    ///    .fold(None, |current, d| Some(clf.fit_with(current, &d)))
    ///    .unwrap();
    ///
    /// let pred = model.as_ref().unwrap().predict(x.view());
    ///
    /// assert_eq!(pred, y);
    /// # Ok(())
    /// # }
    /// ```
    fn fit_with(&self, model_in: I, dataset: &Dataset<ArrayView2<A>, L>) -> Self::Object {
        let x = dataset.records();
        let y = dataset.targets();

        //propagate errors
        let model_in = match model_in.into() {
            Some(Err(err)) => return Err(err),
            Some(Ok(x)) => Some(x),
            None => None,
        };

        // If the ratio of the variance between dimensions is too small, it will cause
        // numerical errors. We address this by artificially boosting the variance
        // by `epsilon` (a small fraction of the variance of the largest feature)
        let epsilon =
            A::from(self.var_smoothing).unwrap() * *x.var_axis(Axis(0), A::zero()).max()?;

        let mut model = match model_in {
            Some(mut temp) => {
                temp.class_info
                    .values_mut()
                    .for_each(|x| x.sigma -= epsilon);
                temp
            }
            None => GaussianNb {
                class_info: HashMap::new(),
            },
        };

        let yunique = y.labels();

        for class in yunique.iter() {
            // We filter x for records that correspond to the current class
            let xclass = Self::filter(&x, y.as_slice(), *class);

            // We count the number of occurances of the class
            let nclass = xclass.nrows();

            // We compute the update of the gaussian mean and variance
            let mut class_info = model
                .class_info
                .entry(*class)
                .or_insert_with(ClassInfo::default);
            let (theta_new, sigma_new) = Self::update_mean_variance(
                class_info.class_count,
                &class_info.theta.view(),
                &class_info.sigma.view(),
                &xclass,
            );

            // We now update the mean, variance and class count
            class_info.theta = theta_new;
            class_info.sigma = sigma_new;
            class_info.class_count += nclass;
        }

        // We add back the epsilon previously subtracted for numerical
        // calculation stability
        model
            .class_info
            .values_mut()
            .for_each(|x| x.sigma += epsilon);

        // We update the priors
        let class_count_sum = model
            .class_info
            .values()
            .fold(0, |acc, x| acc + x.class_count);
        for info in model.class_info.values_mut() {
            info.prior = A::from(info.class_count).unwrap() / A::from(class_count_sum).unwrap();
        }

        Ok(model)
    }
}

impl GaussianNbParams {
    // Compute online update of gaussian mean and variance
    fn update_mean_variance<A: Float>(
        count_old: usize,
        mu_old: &ArrayView1<A>,
        var_old: &ArrayView1<A>,
        x_new: &Array2<A>,
    ) -> (Array1<A>, Array1<A>) {
        // If incoming data is empty no updates required
        if x_new.nrows() == 0 {
            return (mu_old.to_owned(), var_old.to_owned());
        }

        let count_new = x_new.nrows();

        // unwrap is safe because None is returned only when number of records
        // along the specified axis is 0, we return early if we have o rows
        let mu_new = x_new.mean_axis(Axis(0)).unwrap();

        let var_new = x_new.var_axis(Axis(0), A::zero());

        // If previous batch was empty, we send the new mean and variance calculated
        if count_old == 0 {
            return (mu_new, var_new);
        }

        let count_total = count_old + count_new;

        // Combine old and new mean, taking into consideration the number
        // of observations
        let mu_new_weighted = &mu_new * A::from(count_new).unwrap();
        let mu_old_weighted = mu_old * A::from(count_old).unwrap();
        let mu_weighted =
            (mu_new_weighted + mu_old_weighted).mapv(|x| x / A::from(count_total).unwrap());

        // Combine old and new variance, taking into consideration the number
        // of observations. this is achieved by combining the sum of squared
        // differences
        let ssd_old = var_old * A::from(count_old).unwrap();
        let ssd_new = var_new * A::from(count_new).unwrap();
        let weight = A::from(count_new * count_old).unwrap() / A::from(count_total).unwrap();
        let ssd_weighted = ssd_old + ssd_new + (mu_old - &mu_new).mapv(|x| weight * x.powi(2));
        let var_weighted = ssd_weighted.mapv(|x| x / A::from(count_total).unwrap());

        (mu_weighted, var_weighted)
    }

    // Returns a subset of x corresponding to the class specified by `ycondition`
    fn filter<A: Float>(x: &ArrayView2<A>, y: &[usize], ycondition: usize) -> Array2<A> {
        // We identify the row numbers corresponding to the class we are interested in
        let index = y
            .iter()
            .enumerate()
            .filter_map(|(i, y)| {
                if ycondition == *y {
                    return Some(i);
                }
                None
            })
            .collect::<Vec<_>>();

        // We subset x to only records corresponding to the class represented in `ycondition`
        let mut xsubset = Array2::zeros((index.len(), x.ncols()));
        index
            .iter()
            .enumerate()
            .for_each(|(i, &r)| xsubset.row_mut(i).assign(&x.slice(s![r, ..])));

        xsubset
    }
}

/// Fitted GaussianNB for predicting classes
#[derive(Debug, Clone)]
pub struct GaussianNb<A> {
    class_info: HashMap<usize, ClassInfo<A>>,
}

#[derive(Debug, Default, Clone)]
struct ClassInfo<A> {
    class_count: usize,
    prior: A,
    theta: Array1<A>,
    sigma: Array1<A>,
}

impl<A: Float> Predict<ArrayView2<'_, A>, Array1<usize>> for GaussianNb<A> {
    /// Perform classification on incoming array
    ///
    /// __Panics__ if the input is empty or if pairwise orderings are undefined
    /// (this occurs in presence of NaN values)
    fn predict(&self, x: ArrayView2<'_, A>) -> Array1<usize> {
        let joint_log_likelihood = self.joint_log_likelihood(x);

        // We store the classes and likelihood info in an vec and matrix
        // respectively for easier identification of the dominant class for
        // each input
        let nclasses = joint_log_likelihood.keys().len();
        let n = x.nrows();
        let mut classes = Vec::with_capacity(nclasses);
        let mut likelihood = Array2::zeros((nclasses, n));
        joint_log_likelihood
            .iter()
            .enumerate()
            .for_each(|(i, (&&key, value))| {
                classes.push(key);
                likelihood.row_mut(i).assign(value);
            });

        // Identify the class with the maximum log likelihood
        likelihood.map_axis(Axis(0), |x| {
            let i = x.argmax().unwrap();
            *classes.get(i).unwrap()
        })
    }
}

impl<A: Float> GaussianNb<A> {
    // Compute unnormalized posterior log probability
    fn joint_log_likelihood(&self, x: ArrayView2<A>) -> HashMap<&usize, Array1<A>> {
        let mut joint_log_likelihood = HashMap::new();

        for (class, info) in self.class_info.iter() {
            let jointi = info.prior.ln();

            let mut nij = info
                .sigma
                .mapv(|x| A::from(2. * std::f64::consts::PI).unwrap() * x)
                .mapv(|x| x.ln())
                .sum();
            nij = A::from(-0.5).unwrap() * nij;

            let nij = ((x.to_owned() - &info.theta).mapv(|x| x.powi(2)) / &info.sigma)
                .sum_axis(Axis(1))
                .mapv(|x| x * A::from(0.5).unwrap())
                .mapv(|x| nij - x);

            joint_log_likelihood.insert(class, nij + jointi);
        }

        joint_log_likelihood
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use approx::assert_abs_diff_eq;
    use linfa::Dataset;
    use ndarray::array;

    #[test]
    fn test_gaussian_nb() {
        let x = array![
            [-2., -1.],
            [-1., -1.],
            [-1., -2.],
            [1., 1.],
            [1., 2.],
            [2., 1.]
        ];
        let y = array![1, 1, 1, 2, 2, 2];

        let clf = GaussianNbParams::params();
        let data = Dataset::new(x.view(), y.view());
        let fitted_clf = clf.fit(&data).unwrap();
        let pred = fitted_clf.predict(x.view());
        println!("{:#?}", pred);
        assert_eq!(pred, y);

        let jll = fitted_clf.joint_log_likelihood(x.view());
        let mut expected = HashMap::new();
        expected.insert(
            &1usize,
            array![
                -2.276946847943017,
                -1.5269468546930165,
                -2.276946847943017,
                -25.52694663869301,
                -38.27694652394301,
                -38.27694652394301
            ],
        );
        expected.insert(
            &2usize,
            array![
                -38.27694652394301,
                -25.52694663869301,
                -38.27694652394301,
                -1.5269468546930165,
                -2.276946847943017,
                -2.276946847943017
            ],
        );

        assert_eq!(jll, expected);
    }

    #[test]
    fn test_gnb_fit_with() {
        let x = array![
            [-2., -1.],
            [-1., -1.],
            [-1., -2.],
            [1., 1.],
            [1., 2.],
            [2., 1.]
        ];
        let y = array![1, 1, 1, 2, 2, 2];

        let clf = GaussianNbParams::params();

        let model = x
            .axis_chunks_iter(Axis(0), 2)
            .zip(y.axis_chunks_iter(Axis(0), 2))
            .map(|(a, b)| Dataset::new(a, b))
            .fold(None, |current, d| Some(clf.fit_with(current, &d)))
            .unwrap();

        let pred = model.as_ref().unwrap().predict(x.view());

        assert_eq!(pred, y);

        let jll = model.as_ref().unwrap().joint_log_likelihood(x.view());

        let mut expected = HashMap::new();
        expected.insert(
            &1usize,
            array![
                -2.276946847943017,
                -1.5269468546930165,
                -2.276946847943017,
                -25.52694663869301,
                -38.27694652394301,
                -38.27694652394301
            ],
        );
        expected.insert(
            &2usize,
            array![
                -38.27694652394301,
                -25.52694663869301,
                -38.27694652394301,
                -1.5269468546930165,
                -2.276946847943017,
                -2.276946847943017
            ],
        );

        for (key, value) in jll.iter() {
            assert_abs_diff_eq!(value, expected.get(key).unwrap(), epsilon = 1e-6);
        }
    }
}
