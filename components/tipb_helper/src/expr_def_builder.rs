// Copyright 2019 TiKV Project Authors. Licensed under Apache-2.0.

use codec::prelude::NumberEncoder;
use tidb_query_datatype::{
    FieldTypeAccessor, FieldTypeFlag, FieldTypeTp,
    codec::mysql::{Decimal, DecimalEncoder, Duration, TimeType},
};
use tipb::{Expr, ExprType, FieldType, ScalarFuncSig};

/// A helper utility to build `tipb::Expr` (a.k.a. expression definition)
/// easily.
pub struct ExprDefBuilder(Expr);

impl ExprDefBuilder {
    pub fn constant_int(v: i64) -> Self {
        let mut expr = Expr::default();
        expr.set_tp(ExprType::Int64);
        expr.mut_val().write_i64(v).unwrap();
        expr.mut_field_type()
            .as_mut_accessor()
            .set_tp(FieldTypeTp::LongLong);
        Self(expr)
    }

    pub fn constant_uint(v: u64) -> Self {
        let mut expr = Expr::default();
        expr.set_tp(ExprType::Uint64);
        expr.mut_val().write_u64(v).unwrap();
        expr.mut_field_type()
            .as_mut_accessor()
            .set_tp(FieldTypeTp::LongLong)
            .set_flag(FieldTypeFlag::UNSIGNED);
        Self(expr)
    }

    pub fn constant_real(v: f64) -> Self {
        let mut expr = Expr::default();
        expr.set_tp(ExprType::Float64);
        expr.mut_val().write_f64(v).unwrap();
        expr.mut_field_type()
            .as_mut_accessor()
            .set_tp(FieldTypeTp::Double);
        expr.mut_field_type().set_decimal(-1);
        Self(expr)
    }

    pub fn constant_bytes(v: Vec<u8>) -> Self {
        let mut expr = Expr::default();
        expr.set_tp(ExprType::String);
        expr.set_val(v);
        expr.mut_field_type()
            .as_mut_accessor()
            .set_tp(FieldTypeTp::VarChar);
        Self(expr)
    }

    pub fn constant_decimal(v: Decimal) -> Self {
        let mut expr = Expr::default();
        expr.set_tp(ExprType::MysqlDecimal);
        let (prec, frac) = v.prec_and_frac();
        expr.mut_val().write_decimal(&v, prec, frac).unwrap();
        expr.mut_field_type()
            .as_mut_accessor()
            .set_tp(FieldTypeTp::NewDecimal);
        Self(expr)
    }

    pub fn constant_time(v: u64, time_type: TimeType) -> Self {
        let mut expr = Expr::default();
        expr.set_tp(ExprType::MysqlTime);
        expr.mut_val().write_u64(v).unwrap();
        let tp = match time_type {
            TimeType::Date => FieldTypeTp::Date,
            TimeType::DateTime => FieldTypeTp::DateTime,
            TimeType::Timestamp => FieldTypeTp::Timestamp,
        };
        expr.mut_field_type().as_mut_accessor().set_tp(tp);
        Self(expr)
    }

    pub fn constant_duration(v: Duration) -> Self {
        let mut expr = Expr::default();
        expr.set_tp(ExprType::MysqlDuration);
        expr.mut_val().write_i64(v.to_nanos()).unwrap();
        expr.mut_field_type()
            .as_mut_accessor()
            .set_tp(FieldTypeTp::Duration);
        Self(expr)
    }

    pub fn constant_null(field_type: impl Into<FieldType>) -> Self {
        let mut expr = Expr::default();
        expr.set_tp(ExprType::Null);
        expr.set_field_type(field_type.into());
        Self(expr)
    }

    pub fn column_ref(offset: usize, field_type: impl Into<FieldType>) -> Self {
        let mut expr = Expr::default();
        expr.set_tp(ExprType::ColumnRef);
        expr.mut_val().write_i64(offset as i64).unwrap();
        expr.set_field_type(field_type.into());
        Self(expr)
    }

    pub fn scalar_func(sig: ScalarFuncSig, field_type: impl Into<FieldType>) -> Self {
        let mut expr = Expr::default();
        expr.set_tp(ExprType::ScalarFunc);
        expr.set_sig(sig);
        expr.set_field_type(field_type.into());
        Self(expr)
    }

    pub fn aggr_func(tp: ExprType, field_type: impl Into<FieldType>) -> Self {
        let mut expr = Expr::default();
        expr.set_tp(tp);
        expr.set_field_type(field_type.into());
        Self(expr)
    }

    #[must_use]
    pub fn push_child(mut self, child: impl Into<Expr>) -> Self {
        self.0.mut_children().push(child.into());
        self
    }

    /// Builds the expression definition.
    pub fn build(self) -> Expr {
        self.0
    }
}

impl From<ExprDefBuilder> for Expr {
    fn from(expr_def_builder: ExprDefBuilder) -> Expr {
        expr_def_builder.build()
    }
}
