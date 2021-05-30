//! umap-like embedding from GrapK

#![allow(dead_code)]
// #![recursion_limit="256"]

use num_traits::{Float, NumAssign};
use std::collections::HashMap;

use ndarray::{Array1, Array2, ArrayView1, Axis};
use ndarray_linalg::{Lapack, Scalar};
use sprs::{CsMat, TriMatBase};

// threading needs
use rayon::prelude::*;
use parking_lot::{RwLock};
use std::sync::Arc;

use std::time::Duration;
use cpu_time::ProcessTime;

use crate::fromhnsw::*;
use crate::tools::svdapprox::*;


/// do not consider probabilities under PROBA_MIN, thresolded!!
const PROBA_MIN: f32 = 1.0E-5;


// We need this structure to compute entropy od neighbour distribution
/// This structure stores gathers parameters of a node:
///  - its local scale
///  - list of edges. The f32 field constains distance or directed proba of edge going out of each node
///    (distance and proba) to its nearest neighbours as referenced in field neighbours of KGraph.
///
/// Identity of neighbour node must be fetched in KGraph structure to spare memory
#[derive(Clone)]
struct NodeParam {
    scale: f32,
    edges: Vec<OutEdge<f32>>,
}

impl NodeParam {
    pub fn new(scale: f32, edges: Vec<OutEdge<f32>>) -> Self {
        NodeParam { scale, edges }
    }
} // end of NodeParam

/// We maintain NodeParam for each node as it enables scaling in the embedded space and cross entropy minimization.
struct NodeParams {
    params: Vec<NodeParam>,
}

impl NodeParams {
    pub fn get_node_param(&self, node: NodeIdx) -> &NodeParam {
        return &self.params[node];
    }
} // end of NodeParams




//==================================================================================================================


/// We use a normalized symetric laplacian to go to the svd.
/// But we want the left eigenvectors of the normalized R(andom)W(alk) laplacian so we must keep track
/// of degrees (rown norms)
struct LaplacianGraph {
    sym_laplacian: MatRepr<f32>,
    degrees: Array1<f32>,
}

/// The structure corresponding to the embedding process
/// It must be initiamized by the graph extracted from Hnsw according to the choosen strategy
/// and the asked dimension for embedding
pub struct Embedder<'a,F> {
    /// graph constrcuted with fromhnsw module
    kgraph: &'a KGraph<F>,
    /// the embedding dimension
    asked_dimension: usize,
    /// tells if we used approximated svd (with CSR mode)
    approximated_svd : bool,
    /// contains edge probabilities according to the probabilized graph constructed before laplacian symetrization
    /// It is this representation that is used for cross entropy optimization!
    initial_space: Option<NodeParams>,
    ///
    embedding: Option<Array2<F>>,
} // end of Embedder


impl<'a,F> Embedder<'a,F>
where
    F: Float + Lapack + Scalar + ndarray::ScalarOperand + Send + Sync,
{
    /// constructor from a graph and asked embedding dimension
    pub fn new(kgraph : &'a KGraph<F>, asked_dimension : usize) -> Self {
        Embedder::<F>{kgraph, asked_dimension, approximated_svd : false, initial_space:None, embedding:None}
    } // end of new



    /// do the embedding
    pub fn embed(&mut self) -> Result<usize, usize> {
        // initial embedding via diffusion maps
        let initial_embedding = self.get_dmap_initial_embedding(self.asked_dimension);
        // we nedd to construct field initial_space has been contructed in get_laplacian 
        // cross entropy optimization
        let b : f32 = 1.;
        let _embedding = self.entropy_optimize(b, &initial_embedding);

        //
        Ok(1)
    } /// end embed


    /// returns the embedded data
    pub fn get_emmbedded(&self) -> Option<&Array2<F>> {
        return self.embedding.as_ref();
    }

    // this function initialize and returns embedding by a svd (or else?)
    // We are intersested in first eigenvalues (excpeting 1.) of transition probability matrix
    // i.e last non null eigenvalues of laplacian matrix!!
    // It is in fact diffusion Maps at time 0
    //
    fn get_dmap_initial_embedding(&mut self, asked_dim: usize) -> Array2<F> {
        // get eigen values of normalized symetric lapalcian
        let laplacian = self.get_laplacian();
        log::trace!("got laplacian, going to svd ...");
        let mut svdapprox = SvdApprox::new(&laplacian.sym_laplacian);
        // TODO adjust epsil ?
        let svdmode = RangeApproxMode::EPSIL(RangePrecision::new(0.1, 5, asked_dim + 5));
        let svd_res = svdapprox.direct_svd(svdmode);
        log::trace!("exited svd");
        if !svd_res.is_ok() {
            println!("svd approximation failed");
            std::panic!();
        }
        // As we used a laplacian and probability transitions we eigenvectors corresponding to lower eigenvalues
        let lambdas = svdapprox.get_sigma().as_ref().unwrap();
        // singular vectors are stored in decrasing order according to lapack for both gesdd and gesvd. 
        if lambdas.len() > 2 && lambdas[1] > lambdas[0] {
            panic!("svd spectrum not decreasing");
        }
        // we examine spectrum
        // get first non null lamba
        // lowest lambda should be near 0. effect of laplacian graph.
        log::info!("highest laplacian eigenvalue value : {}", lambdas[lambdas.len() - 1]);
        let first_non_zero_from_end : usize;
        if !self.approximated_svd {
            assert!((lambdas[lambdas.len() - 1]).abs() < 1.0E-4);
            let first_non_zero_opt = lambdas.iter().rev().position(|&x| x > 1.0E-5);
            if !first_non_zero_opt.is_some() {
                println!("cannot find positive eigenvalue");
                std::panic!();
            }
            else {
                first_non_zero_from_end = first_non_zero_opt.unwrap();
            }
        }
        else { // csr case
            first_non_zero_from_end = 0;
        }
        //
        let first_non_zero = lambdas.len() - 1 - first_non_zero_from_end; // is in [0..len()-1]
        log::info!(
            "laplacian last non null eigenvalue at rank : {}, value : {}",
            first_non_zero,
            lambdas[first_non_zero]
        );
        // get info on spectral gap
        if first_non_zero > 0 {
            log::info!("\n first non null eigenvalue rank : {} value : {}", first_non_zero, lambdas[first_non_zero]);
            log::info!("\n next non null eigenvalue rank : {} value : {} \n", first_non_zero-1, lambdas[first_non_zero-1]);
            let rank_dim = asked_dim.min(first_non_zero);
            log::info!("\n eigenvalue at asked_dim : {} value : {} \n", first_non_zero-rank_dim, lambdas[first_non_zero-rank_dim]);
        }
        if lambdas[0] > 1. {
            log::error!("highest laplacian eigenvalue value : {}", lambdas[0]);
        }
        log::info!("\n highest eigenvalue value : {}", lambdas[0]);
        //
        assert!(first_non_zero >= asked_dim);
        let max_dim = asked_dim.min(first_non_zero + 1); // is in [1..len()]
        // We get U at index in range first_non_zero-max_dim..first_non_zero
        let u = svdapprox.get_u().as_ref().unwrap();
        log::debug!("u shape : nrows: {} ,  ncols : {} ", u.nrows(), u.ncols());
        // we can get svd from approx range so that nrows and ncols can be number of nodes!
        let mut embedded = Array2::<F>::zeros((u.nrows(), max_dim));
        // according to theory (See Luxburg or Lafon-Keller diffusion maps) we must go back to eigen vectors of rw laplacian.
        // moreover we must get back to type F
        let sum_diag = laplacian.degrees.into_iter().sum::<f32>().sqrt();
        let j_weights: Vec<f32> = laplacian
            .degrees
            .into_iter()
            .map(|x| x.sqrt() / sum_diag)
            .collect();
        for i in 0..u.nrows() {
            let row_i = u.row(i);
            for j in 0..asked_dim {
                // divide j value by diagonal and convert to F
                embedded[[i, j]] =
                    F::from_f32(row_i[first_non_zero - j] / j_weights[i]).unwrap();
            }
        }
        log::trace!("ended get_dmap_initial_embedding");
        return embedded;
    } // end of get_initial_embedding



    // the function computes a symetric laplacian graph for svd.
    // We will then need the lower non zero eigenvalues and eigen vectors.
    // The best justification for this is in Diffusion Maps.
    //
    // Store in a symetric matrix representation dense of CsMat with for spectral embedding
    // Do the Svd to initialize embedding. After that we do not need any more a full matrix.
    //      - Get maximal incoming degree and choose either a CsMat or a dense Array2.
    //
    // Let x a point y_i its neighbours
    //     after simplification weight assigned can be assumed to be of the form exp(-alfa * (d(x, y_i))
    //     the problem is : how to choose alfa, this is done in get_scale_from_proba_normalisation
    // See also Veerman A Primer on Laplacian Dynamics in Directed Graphs 2020 arxiv https://arxiv.org/abs/2002.02605

    fn get_laplacian(&mut self) -> LaplacianGraph {
        log::trace!("in Embedder::get_laplacian");
        //
        let nbnodes = self.kgraph.get_nb_nodes();
        // get stats
        let max_nbng = self.kgraph.get_max_nbng();
        let mut node_params = Vec::<NodeParam>::with_capacity(nbnodes);
        // TODO define a threshold for dense/sparse representation
        if nbnodes <= 300 {
            log::debug!("Embedder using full matrix");
            let mut transition_proba = Array2::<f32>::zeros((nbnodes, nbnodes));
            // we loop on all nodes, for each we want nearest neighbours, and get scale of distances around it
            // TODO can be // with rayon taking care of indexation
            let neighbour_hood = self.kgraph.get_neighbours();
            for i in 0..neighbour_hood.len() {
                // remind to index each request
                log::trace!(" scaling node {}", i);
                let node_param = self.get_scale_from_proba_normalisation(&neighbour_hood[i]);
                assert_eq!(node_param.edges.len(), neighbour_hood[i].len());
                // CAVEAT diagonal transition 0. or 1. ? Choose 0. as in t-sne umap LargeVis
                transition_proba[[i, i]] = 0.;
                for j in 0..node_param.edges.len() {
                    let edge = node_param.edges[j];
                    transition_proba[[i, edge.node]] = edge.weight;
                } // end of for j
                node_params.push(node_param);
            } // end for i
            log::trace!("scaling of nodes done");            
             // do not forget to store this representation of initial space as we need it for entropy optimization
            self.initial_space = Some(NodeParams{params:node_params});
            // now we symetrize the graph by taking mean
            // The UMAP formula (p_i+p_j - p_i *p_j) implies taking the non null proba when one proba is null,
            // so UMAP initialization is more packed.
            let mut symgraph = (&transition_proba + &transition_proba.view().t()) * 0.5;
            // now we go to the symetric laplacian. compute sum of row and renormalize. See Lafon-Keller-Coifman
            // Diffusions Maps appendix B
            // IEEE TRANSACTIONS ON PATTERN ANALYSIS AND MACHINE INTELLIGENCE,VOL. 28, NO. 11,NOVEMBER 2006
            let diag = symgraph.sum_axis(Axis(1));
            for i in 0..nbnodes {
                let mut row = symgraph.row_mut(i);
                for j in 0..nbnodes {
                    row[[j]] /= -(diag[[i]] * diag[[j]]).sqrt();
                }
                row[[i]] += 1.;      // in fact we take the laplacian to get excatly this!
            }
            //
            log::trace!("\n allocating full matrix laplacian");            
            let laplacian = LaplacianGraph {
                sym_laplacian: MatRepr::from_array2(symgraph),
                degrees: diag,
            };
            laplacian
        } else {
            log::debug!("Embedder using csr matrix");
            self.approximated_svd = true;
            // now we must construct a CsrMat to store the symetrized graph transition probablity to go svd.
            // and initialize field initial_space with some NodeParams
            let neighbour_hood = self.kgraph.get_neighbours();
            // TODO This can be made // with a chashmap
            let mut edge_list = HashMap::<(usize, usize), f32>::with_capacity(nbnodes * max_nbng);
            for i in 0..neighbour_hood.len() {
                let node_param = self.get_scale_from_proba_normalisation(&neighbour_hood[i]);
                assert_eq!(node_param.edges.len(), neighbour_hood[i].len());
                for j in 0..neighbour_hood[i].len() {
                    let edge = neighbour_hood[i][j];
                    edge_list.insert((i, edge.node), node_param.edges[j].weight);
                } // end of for j
                node_params.push(node_param);
            }
            // do not forget to store this representation of initial space as we need it for entropy optimization
            self.initial_space = Some(NodeParams{params:node_params});
            // now we iter on the hasmap symetrize the graph, and insert in triplets transition_proba
            let mut diagonal = Array1::<f32>::zeros(nbnodes);
            let mut rows = Vec::<usize>::with_capacity(nbnodes * 2 * max_nbng);
            let mut cols = Vec::<usize>::with_capacity(nbnodes * 2 * max_nbng);
            let mut values = Vec::<f32>::with_capacity(nbnodes * 2 * max_nbng);

            for ((i, j), val) in edge_list.iter() {
                assert!(*i != *j);  // we do not store null distance for self (loop) edge, its proba transition is always set to 0. CAVEAT
                let sym_val;
                if let Some(t_val) = edge_list.get(&(*j, *i)) {
                    sym_val = (val + t_val) * 0.5;
                } else {
                    sym_val = *val;
                }
                diagonal[*i] += sym_val;
                rows.push(*i);
                cols.push(*j);
                values.push(sym_val);
                diagonal[*i] += sym_val;
                //
                rows.push(*j);
                cols.push(*i);
                values.push(sym_val);
                diagonal[*j] += sym_val;
            }
            // now we push terms (i,i) in csr
            for i in 0..nbnodes {
                rows.push(i);
                cols.push(i);
                values.push(0.);
            }
            // Now we reset non diagonal terms to I-D^-1/2 G D^-1/2  i.e 1. - val[i,j]/(D[i]*D[j])^1/2
            for i in 0..rows.len() {
                let row = rows[i];
                let col = cols[i];
                if row == col {
                    values[i] = 1. - values[i] / (diagonal[row] * diagonal[col]).sqrt();
                }
                else {
                    values[i] = - values[i] / (diagonal[row] * diagonal[col]).sqrt();
                }
            }
            // 
            log::trace!("allocating csr laplacian");            
            let laplacian = TriMatBase::<Vec<usize>, Vec<f32>>::from_triplets(
                (nbnodes, nbnodes),
                rows,
                cols,
                values,
            );
            let csr_mat: CsMat<f32> = laplacian.to_csr();
            let laplacian = LaplacianGraph {
                sym_laplacian: MatRepr::from_csrmat(csr_mat),
                degrees: diagonal,
            };
            laplacian
        } // end case CsMat
          //
    } // end of into_matrepr_for_svd


    // given neighbours of a node we choose scale to satisfy a normalization constraint.
    // p_i = exp[- beta * (d(x,y_i) - d(x, y_1)/ local_scale ]
    // We return beta/local_scale
    // as function is monotonic with respect to scale, we use dichotomy.
    fn get_scale_from_umap(&self, norm: f64, neighbours: &Vec<OutEdge<F>>) -> (f32, Vec<f32>) {
        // p_i = exp[- beta * (d(x,y_i)/ local_scale) ]
        let nbgh = neighbours.len();
        let rho_x = neighbours[0].weight.to_f32().unwrap();
        let mut dist = neighbours
            .iter()
            .map(|n| n.weight.to_f32().unwrap())
            .collect::<Vec<f32>>();
        let f = |beta: f32| {
            dist.iter()
                .map(|d| (-(d - rho_x) * beta).exp())
                .sum::<f32>()
        };
        // f is decreasing
        // TODO we could also normalize as usual?
        let beta = dichotomy_solver(false, f, 0f32, f32::MAX, norm as f32);
        // TODO get quantile info on beta or corresponding entropy ? β should be not far from 1?
        // reuse rho_y_s to return proba of edge
        for i in 0..nbgh {
            dist[i] = (-(dist[i] - rho_x) * beta).exp();
        }
        // in this state neither sum of proba adds up to 1 neither is any entropy (Shannon or Renyi) normailed.
        (1. / beta, dist)
    } // end of get_scale_from_umap


    // Simplest function where we know really what we do and why. Get a normalized proba with constraint.
    // given neighbours of a node we choose scale to satisfy a normalization constraint.
    // p_i = exp[- beta * (d(x,y_i)/ local_scale)]  and then normalized to 1.
    // local_scale can be adjusted so that ratio of last proba to first proba >= epsil.
    // This function returns the local scale (i.e mean distance of a point to its nearest neighbour)
    // and vector of proba weight to nearest neighbours. Min
    fn get_scale_from_proba_normalisation(&self, neighbours: &Vec<OutEdge<F>>) -> NodeParam {
//        log::trace!("in Embedder::get_scale_from_proba_normalisation");
        // p_i = exp[- beta * (d(x,y_i)/ local_scale) * lambda]
        let nbgh = neighbours.len();
        // determnine mean distance to nearest neighbour at local scale
        let rho_x = neighbours[0].weight.to_f32().unwrap();
        let mut rho_y_s = Vec::<f32>::with_capacity(neighbours.len() + 1);
        for i in 0..nbgh {
            let y_i = neighbours[i].node; // y_i is a NodeIx = usize
            rho_y_s.push(self.kgraph.neighbours[y_i][0].weight.to_f32().unwrap());
            // we rho_x, initial_scales
        } // end of for i
        rho_y_s.push(rho_x);
        let mean_rho = rho_y_s.iter().sum::<f32>() / (rho_y_s.len() as f32);
        // we set scale so that transition proba do not vary more than PROBA_MIN between first and last neighbour
        // exp(- (first_dist -last_dist)/scale) >= PROBA_MIN
        // TODO do we need some optimization with respect to this 1 ? as we have lambda for high variations
        let mut scale =  1. * mean_rho;
        assert!(scale > 0.);
        // now we adjust scale so that the ratio of proba of last neighbour to first neighbour do not exceed epsil.
        let first_dist = neighbours[0].weight.to_f32().unwrap();
        let last_dist = neighbours[nbgh - 1].weight.to_f32().unwrap();
        assert!(first_dist > 0. && last_dist > 0.);
        assert!(last_dist >= first_dist);
        //
        if last_dist > first_dist {
            let lambda = (last_dist - first_dist) / (scale * (-PROBA_MIN.ln()));
            if lambda > 1. {
                log::trace!("rescaling with lambda = {}", lambda);
                // we rescale mean_rho to avoid too large range of probabilities in neighbours.
                scale = scale * lambda;
            }
            let mut probas_edge = neighbours
                .iter()
                .map(|n| OutEdge::<f32>::new(n.node, (-n.weight.to_f32().unwrap() / scale).exp()))
                .collect::<Vec<OutEdge<f32>>>();
            //
            // if probas_edge[probas_edge.len() - 1].weight / probas_edge[0].weight > PROBA_MIN {
            //     for i in 0..probas_edge.len() {
            //         println!("edge {} , proba : {} ", neighbours[i].weight.to_f32().unwrap(), probas_edge[i].weight);
            //     }
            // }
            log::trace!("scale : {:.2e} proba gap {:.2e}", scale, probas_edge[probas_edge.len() - 1].weight / probas_edge[0].weight);
            assert!(probas_edge[probas_edge.len() - 1].weight / probas_edge[0].weight >= PROBA_MIN);
            let sum = probas_edge.iter().map(|e| e.weight).sum::<f32>();
            for i in 0..nbgh {
                probas_edge[i].weight = probas_edge[i].weight / sum;
            }
            return NodeParam::new(mean_rho, probas_edge);
        } else {
            // all neighbours are at the same distance!
            let probas_edge = neighbours
                .iter()
                .map(|n| OutEdge::<f32>::new(n.node, 1.0 / nbgh as f32))
                .collect::<Vec<OutEdge<f32>>>();
            return NodeParam::new(scale, probas_edge);
        }
    } // end of get_scale_from_proba_normalisation



    /// get embedding of a given node
    pub fn get_node_embedding(&self, node : NodeIdx) -> ArrayView1<F> {
        self.embedding.as_ref().unwrap().row(node)
    }

    // minimize divergence between embedded and initial distribution probability
    // We use cross entropy as in Umap. The edge weight function must take into acccount an initial density estimate and a scale.
    // The initial density makes the embedded graph asymetric as the initial graph.
    // The optimization function thus should try to restore asymetry and local scale as far as possible.

    fn entropy_optimize(&self, b: f32, initial_embedding : &Array2<F>) -> Result<usize, String> {
        //
        log::info!("in Embedder::entropy_optimize");
        //
        if self.initial_space.is_none() {
            log::error!("Embedder::entropy_optimize : initial_space not constructed, exiting");
            return Err(String::from(" initial_space not constructed, no NodeParams"));
        }
        let ce_optimization = EntropyOptim::new(self.initial_space.as_ref().unwrap(), b, initial_embedding);
        // compute initial value of objective function
        let start = ProcessTime::now();
        let initial_ce = ce_optimization.ce_compute();
        let cpu_time: Duration = start.elapsed();
        println!(" initial cross entropy value {:?},  in time {:?}", initial_ce, cpu_time);
        // We manage some iterations on gradient computing
        let grad_step_init = 0.1;
        //
        log::debug!("in Embedder::entropy_optimize  ... gradient iterations");
        let start = ProcessTime::now();
        for iter in 1..=10 {
            // loop on edges
            let grad_step = grad_step_init/iter as f64;
            let start = ProcessTime::now();
            ce_optimization.gradient_iteration(grad_step);
            let cpu_time: Duration = start.elapsed();
            println!(" gradient iteration time {:?}", cpu_time);
        }
        let cpu_time: Duration = start.elapsed();
        println!(" gradient iterations cpu_time {:?}",  cpu_time);
        let final_ce = ce_optimization.ce_compute();
        println!(" final cross entropy value {:?}", final_ce);
        //
        Ok(1)
    } // end of entropy_optimize



} // end of impl Embedder

//==================================================================================================================


/// All we need to optimize entropy discrepancy
/// A list of edge with its weight, an array of scale for each origin node of an edge, proba (weight) of each edge
/// and coordinates in embedded_space with lock protection for //
struct EntropyOptim<F> {
    /// for each edge , initial node, end node, proba (weight of edge) 24 bytes
    edges : Vec<(NodeIdx, OutEdge<f32>)>,
    /// scale for each node
    initial_scales : Vec<f32>,
    /// embedded coordinates of each node, under RwLock to // optimization     nbnodes * (embedded dim * f32 + lock size))
    embedded : Vec<Arc<RwLock<Array1<F>>>>,
    /// embedded_scales
    embedded_scales : Vec<f32>,
    ///
    b : f32
} // end of EntropyOptim




impl <F> EntropyOptim<F> 
    where F: Float +  NumAssign + std::iter::Sum + num_traits::cast::FromPrimitive + Send + Sync {
    //
    pub fn new(node_params : &NodeParams, b: f32, initial_embed : &Array2<F>) -> Self {
        log::info!("entering EntropyOptim::new");
        //
        let nbng = node_params.params[0].edges.len();
        let nbnodes = node_params.params.len();
        let mut edges = Vec::<(NodeIdx, OutEdge<f32>)>::with_capacity(nbnodes*nbng);
        let mut initial_scales =  Vec::<f32>::with_capacity(nbnodes);
        // construct field edges
        for i in 0..nbnodes {
            initial_scales.push(node_params.params[i].scale);
            for j in 0..node_params.params[i].edges.len() {
                edges.push((i, node_params.params[i].edges[j]));
            }
        }
        // construct embedded, initial embed can be droped now
        let mut embedded = Vec::<Arc<RwLock<Array1<F>>>>::new();
        let nbrow  = initial_embed.nrows();
        for i in 0..nbrow {
            embedded.push(Arc::new(RwLock::new(initial_embed.row(i).to_owned())));
        }
        // compute embedded scales
//        let embedded_scales = estimate_embedded_scales_from_first_neighbour(node_params, b, initial_embed);
        let embedded_scales = estimate_embedded_scale_from_initial_scales(&initial_scales);
        //
        EntropyOptim { edges, initial_scales, embedded, embedded_scales, b}
        // construct field embedded
    }  // end of new 


    // return result as an Array2<F> cloning data to return result to struct Embedder
    fn get_embedded(&mut self) -> Array2<F> {
        let nbrow = self.embedded.len();
        let nbcol = self.embedded[0].read().len();
        let mut embedding_res = Array2::<F>::zeros((nbrow, nbcol));
        // TODO version 0.15 provides move_into and push_row
        for i in 0..nbrow {
            let row = self.embedded[i].read();
            for j in 0..nbcol {
                embedding_res[[i,j]] = row[j];
            }
        }
        return embedding_res;
    }


    // computes croos entropy between initial space and embedded distribution. 
    // necessary to monitor optimization
    fn ce_compute(&self) -> f64 {
        log::info!("entering EntropyOptim::ce_compute");
        //
        let mut ce_entropy = 0.;
        for edge in self.edges.iter() {
            let node_i = edge.0;
            let node_j = edge.1.node;
            let weight_ij = edge.1.weight as f64;
            let weight_ij_embed = cauchy_edge_weight(&self.embedded[node_i].read(), 
                    F::from_f32(self.initial_scales[node_i]).unwrap(), self.b,
                    &self.embedded[node_j].read()).to_f64().unwrap();
            if weight_ij_embed > 0. {
                ce_entropy += -weight_ij * weight_ij_embed.ln();
            }
            if weight_ij_embed < 1. {
                ce_entropy += - (1. - weight_ij) * (1. - weight_ij_embed).ln();
            }            
            if !ce_entropy.is_finite() {
                log::debug!("weight_ij {} weight_ij_embed {}", weight_ij, weight_ij_embed);
                std::panic!();
            }
        }
        //
        ce_entropy
    } // end of ce_compute



    // threaded version for computing cross entropy between initial distribution and embedded distribution with Cauchy law.
    fn ce_compute_threaded(&self) -> f64 {
        log::info!("entering EntropyOptim::ce_compute_threaded");
        //
        let ce_entropy = self.edges.par_iter()
                .fold( || 0.0f64, | entropy : f64, edge| entropy + {
                    let node_i = edge.0;
                    let node_j = edge.1.node;
                    let weight_ij = edge.1.weight as f64;
                    let weight_ij_embed = cauchy_edge_weight(&self.embedded[node_i].read(), 
                            F::from_f32(self.initial_scales[node_i]).unwrap(), self.b,
                            &self.embedded[node_j].read()).to_f64().unwrap();
                    let mut term = 0.;
                    if weight_ij_embed > 0. {
                        term += -weight_ij * weight_ij_embed.ln();
                    }
                    if weight_ij_embed < 1. {
                        term += - (1. - weight_ij) * (1. - weight_ij_embed).ln();
                    }
                    term
                })
                .sum::<f64>();
        return ce_entropy;
    }


    // TODO : pass functions corresponding to edge_weight and grad_edge_weight as arguments to test others weight function
    /// This function optimize cross entropy for Shannon cross entropy
    fn ce_optim_edge_shannon(&self, edge_idx : usize, grad_step : f64)
    where
        F: Float + NumAssign + std::iter::Sum + num_traits::cast::FromPrimitive
    {
        // get coordinate of node
        let node_i = self.edges[edge_idx].0;
        // we locks once and directly a write lock as conflicts should be small, many edges, some threads. see Recht Hogwild!
        let mut y_i = self.embedded[node_i].write();
        let mut gradient = Array1::<F>::zeros(y_i.len());
        //
        let edge_out = self.edges[edge_idx];
        let node_j = edge_out.1.node;
        let weight = edge_out.1.weight;
        assert!(weight <= 1.);
        let scale = self.initial_scales[node_i] as f64;
        let mut y_j = self.embedded[node_j].write();
        // compute l2 norm of y_j - y_i
        let d_ij : f64 = y_i.iter().zip(y_j.iter()).map(|(vi,vj)| (*vi-*vj)*(*vi-*vj)).sum::<F>().to_f64().unwrap();
        // taking into account P and 1.-P 
        let mut coeff_ij = (- weight as f64 / (scale + d_ij)) + 
            ( 1. - weight as f64) / ( (0.001 + d_ij) * (1. + d_ij / scale)  );
        coeff_ij *= 2.;
        // update gradient
        for k in 0..y_i.len() {
            gradient[k] = (y_j[k] - y_i[k]) * F::from_f64(coeff_ij).unwrap();
        }
        // update positions
        for k in 0..y_i.len() {
            y_i[k] += gradient[k] * F::from_f64(grad_step).unwrap();
            y_j[k] -= gradient[k] * F::from_f64(grad_step).unwrap();
        }
    } // end of ce_optim_from_point



    // TODO to be called in // all was done for
    fn gradient_iteration(&self, grad_step : f64) {
        for i in 0..self.edges.len() {
            self.ce_optim_edge_shannon(i, grad_step);
        }
    } // end of gradient_iteration

    
}  // end of impl EntropyOptim






/// computes the weight of an embedded edge.
/// scale correspond at density observed at initial point in original graph (hence the asymetry)
fn cauchy_edge_weight<F>(initial_point: &Array1<F>, scale: F, b : f32, other: &Array1<F>) -> F
where
    F: Float + std::iter::Sum
{
    let mut dist = initial_point
        .iter()
        .zip(other.iter())
        .map(|(i, f)| (*i - *f) * (*i - *f))
        .sum::<F>();
    dist = dist / (scale*scale);
    // ( b^dist = exp(d*log(b) )
    dist = F::from((b*(dist.to_f32().unwrap()).ln()).exp()).unwrap();
    //
    let weight =  F::one() / (F::one() + dist);
    assert!(weight.is_normal());      
    return weight;
} // end of embedded_edge_weight




// gradient of embedded_edge_weight, fills in gradient to avoid allocation in return
fn grad_cauchy_edge_weight<F>(initial_point: &Array1<F>, scale: F, b : f32, other: &Array1<F>,
            gradient: &mut Array1<F>,
) where
    F: Float + std::iter::Sum + num_traits::cast::FromPrimitive,
{
    //
    assert_eq!(gradient.len(), initial_point.len());
    //
    // compute rescaled squared distance as a f32
    let d_ij = (l2_dist(&initial_point.view(), &other.view())/(scale*scale)).to_f32().unwrap();
    let cauchy_weight = cauchy_edge_weight(initial_point, scale, b, initial_point);
    //
    let coeff_f = F::from(2. * b * d_ij.pow(b - 1.)).unwrap() * (cauchy_weight * cauchy_weight);
    for i in 0..gradient.len() {
        gradient[i] = coeff_f * (other[i] - initial_point[i]);
    }
} // end of grad_embedded_weight



fn l2_dist<F>(y1: &ArrayView1<'_, F> , y2 : &ArrayView1<'_, F>) -> F 
where F :  Float + std::iter::Sum + num_traits::cast::FromPrimitive {
    //
    y1.iter().zip(y2.iter()).map(|(v1,v2)| (*v1 - *v2) * (*v1- *v2)).sum()
}  // end of l2_dist



// in this function we compute scale in embedded space so a point is not pushed with respect to what corresponds to
// its first neighbour in original space. what sense
fn estimate_embedded_scales_from_first_neighbour<F> (node_params : &NodeParams, b : f32, initial_embed : &Array2<F>) -> Vec<f32> 
    where F :  Float + std::iter::Sum + num_traits::cast::FromPrimitive {
    let nbnodes = node_params.params.len();
    let mut embedded_scales = Vec::<f32>::with_capacity(nbnodes);
    for i in 0..nbnodes {
        let first_edge = node_params.params[i].edges[0];
        let p1 = first_edge.weight;
        let n1 = first_edge.node;
        let new_scale = (p1/(1.- p1)).pow(1./b) * l2_dist(&initial_embed.row(i), &initial_embed.row(n1)).to_f32().unwrap();
        embedded_scales.push(new_scale);
    }
    embedded_scales
} // end of estimate_scales_from_first_neighbours


// in embedded space (in unit ball) the scale is chosen as the scale at corresponding point / divided by mean initial scales.
fn estimate_embedded_scale_from_initial_scales(initial_scales :&Vec<f32>) -> Vec<f32> {
    let mean_scale : f32 = initial_scales.iter().sum();
    let embedded_scale : Vec<f32> = initial_scales.iter().map(|x| x/mean_scale).collect();
    embedded_scale
}  // end of estimate_embedded_scale_from_initial_scales



/// search a root for f(x) = target between lower_r and upper_r. The flag increasing specifies the variation of f. true means increasing
fn dichotomy_solver<F>(increasing: bool, f: F, lower_r: f32, upper_r: f32, target: f32) -> f32
where
    F: Fn(f32) -> f32,
{
    //
    if lower_r >= upper_r {
        panic!(
            "dichotomy_solver failure low {} greater than upper {} ",
            lower_r, upper_r
        );
    }
    let range_low = f(lower_r).max(f(upper_r));
    let range_upper = f(upper_r).min(f(lower_r));
    if f(lower_r).max(f(upper_r)) < target || f(upper_r).min(f(lower_r)) > target {
        panic!(
            "dichotomy_solver target not in range of function range {}  {} ",
            range_low, range_upper
        );
    }
    //
    if f(upper_r) < f(lower_r) && increasing {
        panic!("f not increasing")
    } else if f(upper_r) > f(lower_r) && !increasing {
        panic!("f not decreasing")
    }
    // target in range, proceed
    let mut middle = 1.;
    let mut upper = upper_r;
    let mut lower = lower_r;
    //
    let mut nbiter = 0;
    while (target - f(middle)).abs() > 1.0E-5 {
        if increasing {
            if f(middle) > target {
                upper = middle;
            } else {
                lower = middle;
            }
        }
        // increasing type
        else {
            // decreasing case
            if f(middle) > target {
                lower = middle;
            } else {
                upper = middle;
            }
        } // end decreasing type
        middle = (lower + upper) * 0.5;
        nbiter += 1;
        if nbiter > 100 {
            panic!(
                "dichotomy_solver do not converge, err :  {} ",
                (target - f(middle)).abs()
            );
        }
    } // end of while
    return middle;
}

mod tests {

//    cargo test embedder  -- --nocapture


    #[allow(unused)]
    use super::*;


    use rand::distributions::{Uniform};
    use rand::prelude::*;


    // have a warning with and error without ?
    #[allow(unused)]
    use hnsw_rs::prelude::*;
    #[allow(unused)]
    use hnsw_rs::hnsw::Neighbour;

    #[allow(unused)]
    fn log_init_test() {
        let _ = env_logger::builder().is_test(true).try_init();
    }


    #[test]
    fn test_dichotomy_inc() {
        let f = |x: f32| x * x;
        //
        let beta = dichotomy_solver(true, f, 0., 5., 2.);
        println!("beta : {}", beta);
        assert!((beta - 2.0f32.sqrt()).abs() < 1.0E-4);
    } // test_dichotomy_inc
    #[test]
    fn test_dichotomy_dec() {
        let f = |x: f32| 1.0f32 / (x * x);
        //
        let beta = dichotomy_solver(false, f, 0.2, 5., 1. / 2.);
        println!("beta : {}", beta);
        assert!((beta - 2.0f32.sqrt()).abs() < 1.0E-4);
    } // test_dichotomy_dec


    #[allow(unused)]
    fn gen_rand_data_f32(nb_elem: usize , dim:usize) -> Vec<Vec<f32>> {
        let mut data = Vec::<Vec<f32>>::with_capacity(nb_elem);
        let mut rng = thread_rng();
        let unif =  Uniform::<f32>::new(0.,1.);
        for i in 0..nb_elem {
            let val = 2. * i as f32 * rng.sample(unif);
            let v :Vec<f32> = (0..dim).into_iter().map(|_|  val * rng.sample(unif)).collect();
            data.push(v);
        }
        data
    }
    
    #[test]
    fn mini_embed_full() {
        log_init_test();
        // generate datz
        let nb_elem = 500;
        let embed_dim = 20;
        let data = gen_rand_data_f32(nb_elem, embed_dim);
        let data_with_id = data.iter().zip(0..data.len()).collect();
        // hnsw construction
        let ef_c = 50;
        let max_nb_connection = 50;
        let nb_layer = 16.min((nb_elem as f32).ln().trunc() as usize);
        let mut hns = Hnsw::<f32, DistL1>::new(max_nb_connection, nb_elem, nb_layer, ef_c, DistL1{});
        // to enforce the asked number of neighbour
        hns.set_keeping_pruned(true);
        hns.parallel_insert(&data_with_id);
        hns.dump_layer_info();
        // go to kgraph
        let knbn = 10;
        let mut kgraph = KGraph::<f32>::new();
        log::info!("calling kgraph.init_from_hnsw_all");
        let res = kgraph.init_from_hnsw_all(&hns, knbn);
        if res.is_err() {
            panic!("init_from_hnsw_all  failed");
        }
        log::info!("minimum number of neighbours {}", kgraph.get_max_nbng());
        let _kgraph_stats = kgraph.get_kraph_stats();
        let embed_dim = 5;
        let mut embedder = Embedder::new(&kgraph, embed_dim);
        let embed_res = embedder.embed();
        assert!(embed_res.is_ok());
    } // end of mini_embed_full



} // end of tests
