//! JavaScript Parsing Functions

mod grammar;

mod arrow;
mod binding;
mod class;
mod declaration;
mod expression;
mod function;
mod module;
mod nota;
pub mod nota_mapping;
mod object;
mod operator;
mod statement;

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub enum Tristate {
    True,
    False,
    Maybe,
}

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub enum FunctionKind {
    Constructor,
    ClassMethod,
    ObjectMethod,
    Declaration,
    Expression,
    DefaultExport,
    TSDeclaration,
}

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub enum VariableDeclarationParent {
    For,
    Statement,
}
