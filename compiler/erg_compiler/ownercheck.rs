use erg_common::color::{GREEN, RESET};
use erg_common::dict::Dict;
use erg_common::error::Location;
use erg_common::log;
use erg_common::set::Set;
use erg_common::traits::{Locational, Stream};
use erg_common::vis::Visibility;
use erg_common::Str;
use Visibility::*;

use erg_type::{ArgsOwnership, HasType, Ownership};

use crate::error::{OwnershipError, OwnershipErrors, OwnershipResult};
use crate::hir::{self, Accessor, Array, Block, Def, Expr, Signature, Tuple, HIR};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum WrapperKind {
    Ref,
    Rc,
    Box,
}

#[derive(Debug, Default)]
struct LocalVars {
    alive_vars: Set<Str>,
    dropped_vars: Dict<Str, Location>,
}

#[derive(Debug)]
pub struct OwnershipChecker {
    path_stack: Vec<(Str, Visibility)>,
    dict: Dict<Str, LocalVars>,
    errs: OwnershipErrors,
}

impl OwnershipChecker {
    pub fn new() -> Self {
        OwnershipChecker {
            path_stack: vec![],
            dict: Dict::new(),
            errs: OwnershipErrors::empty(),
        }
    }

    fn full_path(&self) -> String {
        self.path_stack
            .iter()
            .fold(String::new(), |acc, (path, vis)| {
                if vis.is_public() {
                    acc + "." + &path[..]
                } else {
                    acc + "::" + &path[..]
                }
            })
    }

    // moveされた後の変数が使用されていないかチェックする
    // ProceduralでないメソッドでRefMutが使われているかはSideEffectCheckerでチェックする
    pub fn check(mut self, hir: HIR) -> OwnershipResult<HIR> {
        log!("{GREEN}[DEBUG] the ownership checking process has started.{RESET}");
        self.path_stack.push((hir.name.clone(), Private));
        self.dict
            .insert(Str::from(self.full_path()), LocalVars::default());
        for chunk in hir.module.iter() {
            self.check_expr(chunk, Ownership::Owned);
        }
        log!(
            "{GREEN}[DEBUG] the ownership checking process has completed, found errors: {}{RESET}",
            self.errs.len()
        );
        if self.errs.is_empty() {
            Ok(hir)
        } else {
            Err(self.errs)
        }
    }

    fn check_block(&mut self, block: &Block) {
        for chunk in block.iter() {
            self.check_expr(chunk, Ownership::Owned);
        }
    }

    fn check_expr(&mut self, expr: &Expr, ownership: Ownership) {
        match expr {
            Expr::Def(def) => {
                log!("define: {}", def.sig);
                self.define(def);
                let name = match &def.sig {
                    Signature::Var(var) => {
                        if let Some(name) = var.inspect() {
                            name.clone()
                        } else {
                            Str::ever("::<instant>")
                        }
                    }
                    Signature::Subr(subr) => subr.ident.inspect().clone(),
                };
                self.path_stack.push((name, def.sig.vis()));
                self.dict
                    .insert(Str::from(self.full_path()), LocalVars::default());
                self.check_block(&def.body.block);
                self.path_stack.pop();
            }
            Expr::Accessor(Accessor::Local(local)) => {
                self.check_if_dropped(local.inspect(), local.loc());
                if expr.ref_t().is_mut() && ownership.is_owned() {
                    log!(
                        "drop: {} (in {})",
                        local.inspect(),
                        local.ln_begin().unwrap_or(0)
                    );
                    self.drop(local.inspect(), expr.loc());
                }
            }
            Expr::Accessor(Accessor::Public(public)) => {
                self.check_if_dropped(public.inspect(), public.loc());
                if expr.ref_t().is_mut() && ownership.is_owned() {
                    log!(
                        "drop: {} (in {})",
                        public.inspect(),
                        public.ln_begin().unwrap_or(0)
                    );
                    self.drop(public.inspect(), expr.loc());
                }
            }
            Expr::Accessor(Accessor::Attr(a)) => {
                // REVIEW: is ownership the same?
                self.check_expr(&a.obj, ownership)
            }
            Expr::Accessor(_a) => todo!(),
            // TODO: referenced
            Expr::Call(call) => {
                let args_ownership = call.signature_t().unwrap().args_ownership();
                match args_ownership {
                    ArgsOwnership::Args {
                        self_,
                        non_defaults,
                        defaults,
                    } => {
                        if let Some(self_ownership) = self_ {
                            self.check_expr(&call.obj, self_ownership);
                        }
                        let (nd_ownerships, d_ownerships): (Vec<_>, Vec<_>) = non_defaults
                            .iter()
                            .enumerate()
                            .partition(|(i, _)| *i == call.args.pos_args.len());
                        for (parg, (_, ownership)) in
                            call.args.pos_args.iter().zip(nd_ownerships.into_iter())
                        {
                            self.check_expr(&parg.expr, *ownership);
                        }
                        for (kwarg, (_, ownership)) in call
                            .args
                            .kw_args
                            .iter()
                            .zip(d_ownerships.into_iter().chain(defaults.iter().enumerate()))
                        {
                            self.check_expr(&kwarg.expr, *ownership);
                        }
                    }
                    ArgsOwnership::VarArgs(ownership) => {
                        for parg in call.args.pos_args.iter() {
                            self.check_expr(&parg.expr, ownership);
                        }
                        for kwarg in call.args.kw_args.iter() {
                            self.check_expr(&kwarg.expr, ownership);
                        }
                    }
                    other => todo!("{other:?}"),
                }
            }
            // TODO: referenced
            Expr::BinOp(binop) => {
                self.check_expr(&binop.lhs, ownership);
                self.check_expr(&binop.rhs, ownership);
            }
            Expr::UnaryOp(unary) => {
                self.check_expr(&unary.expr, ownership);
            }
            Expr::Array(array) => match array {
                Array::Normal(arr) => {
                    for a in arr.elems.pos_args.iter() {
                        self.check_expr(&a.expr, ownership);
                    }
                }
                _ => todo!(),
            },
            Expr::Tuple(tuple) => match tuple {
                Tuple::Normal(arr) => {
                    for a in arr.elems.pos_args.iter() {
                        self.check_expr(&a.expr, ownership);
                    }
                }
            },
            Expr::Dict(dict) => match dict {
                hir::Dict::Normal(dic) => {
                    for a in dic.attrs.kw_args.iter() {
                        // self.check_expr(&a.key);
                        self.check_expr(&a.expr, ownership);
                    }
                }
                _ => todo!(),
            },
            Expr::Record(rec) => {
                for def in rec.attrs.iter() {
                    for chunk in def.body.block.iter() {
                        self.check_expr(chunk, ownership);
                    }
                }
            }
            // TODO: capturing
            Expr::Lambda(lambda) => {
                let name_and_vis = (Str::from(format!("<lambda_{}>", lambda.id)), Private);
                self.path_stack.push(name_and_vis);
                self.dict
                    .insert(Str::from(self.full_path()), LocalVars::default());
                self.check_block(&lambda.body);
                self.path_stack.pop();
            }
            _ => {}
        }
    }

    /// TODO: このメソッドを呼ぶとき、スコープを再帰的に検索する
    #[inline]
    fn current_scope(&mut self) -> &mut LocalVars {
        self.dict.get_mut(&self.full_path()[..]).unwrap()
    }

    #[inline]
    fn nth_outer_scope(&mut self, n: usize) -> &mut LocalVars {
        let path = self.path_stack.iter().take(self.path_stack.len() - n).fold(
            String::new(),
            |acc, (path, vis)| {
                if vis.is_public() {
                    acc + "." + &path[..]
                } else {
                    acc + "::" + &path[..]
                }
            },
        );
        self.dict.get_mut(&path[..]).unwrap()
    }

    fn define(&mut self, def: &Def) {
        match &def.sig {
            Signature::Var(sig) => {
                for name in sig.pat.inspects() {
                    self.current_scope().alive_vars.insert(name.clone());
                }
            }
            Signature::Subr(sig) => {
                self.current_scope()
                    .alive_vars
                    .insert(sig.ident.inspect().clone());
            }
        }
    }

    fn drop(&mut self, name: &Str, moved_loc: Location) {
        for n in 0..self.path_stack.len() {
            if self.nth_outer_scope(n).alive_vars.remove(name) {
                self.nth_outer_scope(n)
                    .dropped_vars
                    .insert(name.clone(), moved_loc);
                return;
            }
        }
        panic!("variable not found: {name}");
    }

    fn check_if_dropped(&mut self, name: &Str, loc: Location) {
        for n in 0..self.path_stack.len() {
            if let Some(moved_loc) = self.nth_outer_scope(n).dropped_vars.get(name) {
                let moved_loc = *moved_loc;
                self.errs.push(OwnershipError::move_error(
                    line!() as usize,
                    name,
                    loc,
                    moved_loc,
                    self.full_path(),
                ));
            }
        }
    }
}

impl Default for OwnershipChecker {
    fn default() -> Self {
        Self::new()
    }
}
