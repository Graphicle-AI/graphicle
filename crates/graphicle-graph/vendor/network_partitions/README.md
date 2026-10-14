# Leiden Communities

Leiden University advanced the community identification algorithm first developed by Universite catholique de Louvain in the paper [From Louvain to Leiden: guaranteeing well-connected communities](https://arxiv.org/abs/1810.08473), as well as provided a [Java reference implementation](https://github.com/CWTSLeiden/networkanalysis/) released under the MIT license.

Using this reference implementation as a starting point, this implementation aims to bring the Leiden community detection algorithm to [Rust](https://www.rust-lang.org), which in turn will enable Python bindings to native code as well as WASM compiled versions. There will also be a CLI package for native command line community detection.
