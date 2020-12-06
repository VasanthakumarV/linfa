# How to contribute to the Linfa project

This document should be used as a reference when contributing to Linfa. It describes how an algorithm should be implemented to fit well into the Linfa ecosystem. First, there are implementation details, how to use a generic float type, how to use the `Dataset` type in arguments etc. Second, the cargo manifest should be set up, such that a user can choose for different backends. 

## Datasets and learning traits

An important part of the Linfa ecosystem is how to organize data for the training and estimation process. A [Dataset](src/dataset/mod.rs) serves this purpose. It is a small wrapper of data and targets types and should be used as argument for the [Fit](src/traits.rs) trait. Its parametrization is generic, with [Records](src/dataset/mod.rs) representing input data (atm only implemented for `ndarray::ArrayBase`) and [Targets](src/dataset/mod.rs) for targets.

You can find traits for different classes of algorithms [here](src/traits.rs). For example, to implement a fittable algorithm, which takes a `Kernel` as input data and boolean array as targets:
```rust
impl<'a, F: Float> Fit<'a, Kernel<'a, F>, Vec<bool>> for SvmParams<F, Pr> {
    type Object = Svm<'a, F, Pr>;

    fn fit(&self, dataset: &'a Dataset<Kernel<'a, F>, Vec<bool>>) -> Self::Object {
        ...
    }
}
```
the type of the dataset is `&'a Dataset<Kernel<'a, F>, Vec<bool>>`, ensuring that the kernel lives long enough during the training. It produces a fitted state, called `Svm<'a, F, Pr>` with probability type `Pr`.

The [Predict](src/traits.rs) should be implemented with dataset arguments, as well as arrays. If a dataset is provided, then predict takes its ownership and returns a new dataset with predicted targets. For an array, predict takes a reference and returns predicted targets. In the same context, SVM implemented predict like this:
```rust
impl<'a, F: Float, T: Targets> Predict<Dataset<Array2<F>, T>, Dataset<Array2<F>, Vec<Pr>>>
    for Svm<'a, F, Pr>
{
    fn predict(&self, data: Dataset<Array2<F>, T>) -> Dataset<Array2<F>, Vec<Pr>> {
        ...
    }
}
```
and
```rust
impl<'a, F: Float, D: Data<Elem = F>> Predict<ArrayBase<D, Ix2>, Vec<Pr>> for Svm<'a, F, Pr> {
    fn predict(&self, data: ArrayBase<D, Ix2>) -> Vec<Pr> {
        ...
    }
}
```

For an example of a `Transformer` please look into the [linfa-kernel](linfa-kernel/src/lib.rs) implementation.

## Parameters and builder

An algorithm has a number of hyperparameters, describing how it operates. This section describes how the algorithm's structs should be organized in order to conform with other implementations. 

Imagine we have an implementation of `MyAlg`, there should a separate struct called `MyAlgParams`. The method `MyAlg::params(..) -> MyAlgParams` constructs a parameter set with default parameters and optionally required arguments (for example the number of clusters). If no parameters are required, then `std::default::Default` can be implemented as well:
```rust
impl Default for MyAlg {
    fn default() -> MyAlgParams {
        MyAlg::params()
    }
}
```

The `MyAlgParams` should implement the Consuming Builder pattern, explained in the [Rust Book](https://doc.rust-lang.org/1.0.0/style/ownership/builders.html). Each hyperparameter gets a single field in the struct, as well as a method to modify it. Sometimes a random number generator is used in the training process. Then two separate methods should take a seed or a random number generator. With the seed a default RNG is initialized, for example [Isaac64](https://docs.rs/rand_isaac/0.2.0/rand_isaac/isaac64/index.html).

With a constructed set of parameters, the `MyAlgParams::fit(..) -> Result<MyAlg>` executes the learning process and returns a learned state. If one of the parameters is invalid (for example out of a required range), then an `Error::InvalidState` should be returned. For transformers there is only `MyAlg`, and no `MyAlgParams`, because there is no hidden state to be learned.

Following this convention, the pattern can be used by the user like this:
```rust
MyAlg::params()
    .eps(1e-5)
    .backwards(true)
    ...
    .fit(&dataset)?;
```

## Use a specific backend for testing

When you're implementing tests, which are relying on `ndarray-linalg`, you have to add the `openblas-src` crate. This will instruct cargo to compile the backend, in order to find the required symbols. The `linfa` framework uses the OpenBLAS system library by default, but an additional feature can be used to build the OpenBLAS library while compiling.
```
[features]
default = ["tests-openblas-system"]
tests-openblas-system = ["openblas-src/system"]
tests-openblas-build = ["openblas-src/cblas", "openblas-src/lapacke"]

[dev-dependencies]
...
openblas-src = "0.9" 
```
and you have to add an `extern crate openblas_src` to your the `tests` module.

## Generic float types

Every algorithm should be implemented for `f32` and `f64` floating points. This can be achieved with the `linfa::Float` trait, which is basically just a combination of `ndarray::NdFloat` and `num_traits::Float`. You can look up most of the constants (like zero, one, PI) in the `num_traits` documentation. Here is a small example for a function, generic over `Float`:
```rust
use linfa::Float;
fn div_capped<F: Float>(num: F) {
    F::one() / (num + F::from(1e-5).unwrap())
}
```

## Make serde optionally

If you want to implement `Serialize` and `Deserialize` for your parameters, please do that behind a feature flag. You can add to your cargo manifest
```
[features]
serde = ["serde_crate", "ndarray/serde"]

[dependencies.serde_crate]
package = "serde"
optional = true
version = "1.0"
```
which basically renames the `serde` crate to `serde_crate` and adds a feature `serde`. In your parameter struct, move the macro definition behind the `serde` feature:
```rust
#[cfg(feature = "serde")]
use serde_crate::{Deserialize, Serialize};

#[cfg_attr(
    feature = "serde",
    derive(Serialize, Deserialize),
    serde(crate = "serde_crate")
)]
#[derive(Clone, Debug, PartialEq)]
pub struct HyperParams {
...
}
```

