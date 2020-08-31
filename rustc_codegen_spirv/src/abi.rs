use crate::builder::Builder;
use crate::codegen_cx::CodegenCx;
use rspirv::dr::Operand;
use rspirv::spirv::{Decoration, StorageClass, Word};
use rustc_middle::ty::{layout::TyAndLayout, Ty, TyKind};
use rustc_target::abi::call::{FnAbi, PassMode};
use rustc_target::abi::{Abi, FieldsShape, LayoutOf, Primitive, Scalar, Variants};
use std::fmt;

#[derive(Clone, Debug, PartialEq, Eq, Ord, PartialOrd, Hash)]
pub enum SpirvType {
    Void,
    Bool,
    Integer(u32, bool),
    Float(u32),
    /// This uses the rustc definition of "adt", i.e. a struct, enum, or union
    Adt {
        name: Option<String>,
        // TODO: enums/unions
        field_types: Vec<Word>,
        /// *byte* offsets
        field_offsets: Option<Vec<u32>>,
        field_names: Option<Vec<String>>,
    },
    Vector {
        element: Word,
        count: Word,
    },
    Array {
        element: Word,
        count: Word,
    },
    Pointer {
        storage_class: StorageClass,
        pointee: Word,
    },
    Function {
        return_type: Word,
        arguments: Vec<Word>,
    },
}

fn memset_fill_u16(b: u8) -> u16 {
    b as u16 | ((b as u16) << 8)
}

fn memset_fill_u32(b: u8) -> u32 {
    b as u32 | ((b as u32) << 8) | ((b as u32) << 16) | ((b as u32) << 24)
}

fn memset_fill_u64(b: u8) -> u64 {
    b as u64
        | ((b as u64) << 8)
        | ((b as u64) << 16)
        | ((b as u64) << 24)
        | ((b as u64) << 32)
        | ((b as u64) << 40)
        | ((b as u64) << 48)
        | ((b as u64) << 56)
}

fn memset_dynamic_scalar<'a, 'spv, 'tcx>(
    builder: &Builder<'a, 'spv, 'tcx>,
    fill_var: Word,
    byte_width: usize,
    is_float: bool,
) -> Word {
    let composite_type = SpirvType::Vector {
        element: SpirvType::Integer(8, false).def(builder),
        count: builder.constant_u32(byte_width as u32).def,
    }
    .def(builder);
    let composite = builder
        .emit()
        .composite_construct(
            composite_type,
            None,
            std::iter::repeat(fill_var).take(byte_width),
        )
        .unwrap();
    let result_type = if is_float {
        SpirvType::Float(byte_width as u32 * 8)
    } else {
        SpirvType::Integer(byte_width as u32 * 8, false)
    };
    builder
        .emit()
        .bitcast(result_type.def(builder), None, composite)
        .unwrap()
}

impl SpirvType {
    /// Note: Builder::type_* should be called *nowhere else* but here, to ensure CodegenCx::type_defs stays up-to-date
    pub fn def<'spv, 'tcx>(&self, cx: &CodegenCx<'spv, 'tcx>) -> Word {
        if let Some(&cached) = cx.type_cache.borrow().get(self) {
            return cached;
        }
        let result = match *self {
            SpirvType::Void => cx.emit_global().type_void(),
            SpirvType::Bool => cx.emit_global().type_bool(),
            SpirvType::Integer(width, signedness) => cx
                .emit_global()
                .type_int(width, if signedness { 1 } else { 0 }),
            SpirvType::Float(width) => cx.emit_global().type_float(width),
            SpirvType::Adt {
                ref name,
                ref field_types,
                ref field_offsets,
                ref field_names,
            } => {
                let mut emit = cx.emit_global();
                // Ensure a unique struct is emitted each time, due to possibly having different OpMemberDecorates
                let id = emit.id();
                let result = emit.type_struct_id(Some(id), field_types.iter().cloned());
                if let Some(name) = name {
                    emit.name(result, name);
                }
                if let Some(field_offsets) = field_offsets {
                    for (index, offset) in field_offsets.iter().copied().enumerate() {
                        emit.member_decorate(
                            result,
                            index as u32,
                            Decoration::Offset,
                            [Operand::LiteralInt32(offset)].iter().cloned(),
                        );
                    }
                }
                if let Some(field_names) = field_names {
                    for (index, field_name) in field_names.iter().enumerate() {
                        emit.member_name(result, index as u32, field_name);
                    }
                }
                result
            }
            SpirvType::Vector { element, count } => cx.emit_global().type_vector(element, count),
            SpirvType::Array { element, count } => cx.emit_global().type_array(element, count),
            SpirvType::Pointer {
                storage_class,
                pointee,
            } => cx.emit_global().type_pointer(None, storage_class, pointee),
            SpirvType::Function {
                return_type,
                ref arguments,
            } => cx
                .emit_global()
                .type_function(return_type, arguments.iter().cloned()),
        };
        // Change to expect_none if/when stabilized
        assert!(
            cx.type_defs
                .borrow_mut()
                .insert(result, self.clone())
                .is_none(),
            "type_defs already had entry, caching failed? {:#?}",
            self.clone().debug(cx)
        );
        assert!(
            cx.type_cache
                .borrow_mut()
                .insert(self.clone(), result)
                .is_none(),
            "type_cache already had entry, caching failed? {:#?}",
            self.clone().debug(cx)
        );
        result
    }

    pub fn debug<'cx, 'spv, 'tcx>(
        self,
        cx: &'cx CodegenCx<'spv, 'tcx>,
    ) -> SpirvTypePrinter<'cx, 'spv, 'tcx> {
        SpirvTypePrinter { ty: self, cx }
    }

    pub fn sizeof_in_bits<'spv, 'tcx>(&self, cx: &CodegenCx<'spv, 'tcx>) -> usize {
        match *self {
            SpirvType::Void => 0,
            SpirvType::Bool => 1,
            SpirvType::Integer(width, _) => width as usize,
            SpirvType::Float(width) => width as usize,
            SpirvType::Adt {
                ref field_types, ..
            } => field_types
                .iter()
                .map(|&ty| cx.lookup_type(ty).sizeof_in_bits(cx))
                .sum(),
            SpirvType::Vector { element, count } => {
                cx.lookup_type(element).sizeof_in_bits(cx)
                    * cx.builder.lookup_const_u64(count).unwrap() as usize
            }
            SpirvType::Array { element, count } => {
                cx.lookup_type(element).sizeof_in_bits(cx)
                    * cx.builder.lookup_const_u64(count).unwrap() as usize
            }
            SpirvType::Pointer { .. } => cx.tcx.data_layout.pointer_size.bits() as usize,
            SpirvType::Function { .. } => cx.tcx.data_layout.pointer_size.bits() as usize,
        }
    }

    pub fn memset_const_pattern<'spv, 'tcx>(
        &self,
        cx: &CodegenCx<'spv, 'tcx>,
        fill_byte: u8,
    ) -> Word {
        match *self {
            SpirvType::Void => panic!("TODO: void memset not implemented yet"),
            SpirvType::Bool => panic!("TODO: bool memset not implemented yet"),
            SpirvType::Integer(width, _signedness) => match width {
                8 => cx.builder.constant_u32(self.def(cx), fill_byte as u32),
                16 => cx
                    .builder
                    .constant_u32(self.def(cx), memset_fill_u16(fill_byte) as u32),
                32 => cx
                    .builder
                    .constant_u32(self.def(cx), memset_fill_u32(fill_byte)),
                64 => cx
                    .builder
                    .constant_u64(self.def(cx), memset_fill_u64(fill_byte)),
                _ => panic!("memset on integer width {} not implemented yet", width),
            },
            SpirvType::Float(width) => match width {
                32 => cx
                    .builder
                    .constant_f32(self.def(cx), f32::from_bits(memset_fill_u32(fill_byte))),
                64 => cx
                    .builder
                    .constant_f64(self.def(cx), f64::from_bits(memset_fill_u64(fill_byte))),
                _ => panic!("memset on float width {} not implemented yet", width),
            },
            SpirvType::Adt { .. } => panic!("memset on structs not implemented yet"),
            SpirvType::Vector { element, count } => {
                let elem_pat = cx.lookup_type(element).memset_const_pattern(cx, fill_byte);
                let count = cx.builder.lookup_const_u64(count).unwrap() as usize;
                cx.emit_global()
                    .constant_composite(self.def(cx), vec![elem_pat; count])
            }
            SpirvType::Array { element, count } => {
                let elem_pat = cx.lookup_type(element).memset_const_pattern(cx, fill_byte);
                let count = cx.builder.lookup_const_u64(count).unwrap() as usize;
                cx.emit_global()
                    .constant_composite(self.def(cx), vec![elem_pat; count])
            }
            SpirvType::Pointer { .. } => panic!("memset on pointers not implemented yet"),
            SpirvType::Function { .. } => panic!("memset on functions not implemented yet"),
        }
    }

    pub fn memset_dynamic_pattern<'a, 'spv, 'tcx>(
        &self,
        builder: &Builder<'a, 'spv, 'tcx>,
        fill_var: Word,
    ) -> Word {
        match *self {
            SpirvType::Void => panic!("TODO: void memset not implemented yet"),
            SpirvType::Bool => panic!("TODO: bool memset not implemented yet"),
            SpirvType::Integer(width, _signedness) => match width {
                8 => fill_var,
                16 => memset_dynamic_scalar(builder, fill_var, 2, false),
                32 => memset_dynamic_scalar(builder, fill_var, 4, false),
                64 => memset_dynamic_scalar(builder, fill_var, 8, false),
                _ => panic!("memset on integer width {} not implemented yet", width),
            },
            SpirvType::Float(width) => match width {
                32 => memset_dynamic_scalar(builder, fill_var, 4, true),
                64 => memset_dynamic_scalar(builder, fill_var, 8, true),
                _ => panic!("memset on float width {} not implemented yet", width),
            },
            SpirvType::Adt { .. } => panic!("memset on structs not implemented yet"),
            SpirvType::Array { element, count } | SpirvType::Vector { element, count } => {
                let elem_pat = builder
                    .lookup_type(element)
                    .memset_dynamic_pattern(builder, fill_var);
                let count = builder.builder.lookup_const_u64(count).unwrap() as usize;
                builder
                    .emit()
                    .composite_construct(
                        self.def(builder),
                        None,
                        std::iter::repeat(elem_pat).take(count),
                    )
                    .unwrap()
            }
            SpirvType::Pointer { .. } => panic!("memset on pointers not implemented yet"),
            SpirvType::Function { .. } => panic!("memset on functions not implemented yet"),
        }
    }
}

pub struct SpirvTypePrinter<'cx, 'spv, 'tcx> {
    ty: SpirvType,
    cx: &'cx CodegenCx<'spv, 'tcx>,
}

impl fmt::Debug for SpirvTypePrinter<'_, '_, '_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.ty {
            SpirvType::Void => f.debug_struct("Void").finish(),
            SpirvType::Bool => f.debug_struct("Bool").finish(),
            SpirvType::Integer(width, signedness) => f
                .debug_struct("Integer")
                .field("width", &width)
                .field("signedness", &signedness)
                .finish(),
            SpirvType::Float(width) => f.debug_struct("Float").field("width", &width).finish(),
            SpirvType::Adt {
                ref name,
                ref field_types,
                ref field_offsets,
                ref field_names,
            } => {
                let fields = field_types
                    .iter()
                    .map(|&f| self.cx.debug_type(f))
                    .collect::<Vec<_>>();
                f.debug_struct("Adt")
                    .field("name", &name)
                    .field("field_types", &fields)
                    .field("field_offsets", field_offsets)
                    .field("field_names", field_names)
                    .finish()
            }
            SpirvType::Vector { element, count } => f
                .debug_struct("Vector")
                .field("element", &self.cx.debug_type(element))
                .field(
                    "count",
                    &self
                        .cx
                        .builder
                        .lookup_const_u64(count)
                        .expect("Vector type has invalid count value"),
                )
                .finish(),
            SpirvType::Array { element, count } => f
                .debug_struct("Array")
                .field("element", &self.cx.debug_type(element))
                .field(
                    "count",
                    &self
                        .cx
                        .builder
                        .lookup_const_u64(count)
                        .expect("Array type has invalid count value"),
                )
                .finish(),
            SpirvType::Pointer {
                storage_class,
                pointee,
            } => f
                .debug_struct("Pointer")
                .field("storage_class", &storage_class)
                .field("pointee", &self.cx.debug_type(pointee))
                .finish(),
            SpirvType::Function {
                return_type,
                ref arguments,
            } => {
                let args = arguments
                    .iter()
                    .map(|&a| self.cx.debug_type(a))
                    .collect::<Vec<_>>();
                f.debug_struct("Function")
                    .field("return_type", &self.cx.lookup_type(return_type))
                    .field("arguments", &args)
                    .finish()
            }
        }
    }
}

impl fmt::Display for SpirvTypePrinter<'_, '_, '_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.ty {
            SpirvType::Void => f.write_str("void"),
            SpirvType::Bool => f.write_str("bool"),
            SpirvType::Integer(width, signedness) => {
                let prefix = if signedness { "i" } else { "u" };
                write!(f, "{}{}", prefix, width)
            }
            SpirvType::Float(width) => write!(f, "f{}", width),
            SpirvType::Adt {
                ref name,
                ref field_types,
                field_offsets: _,
                ref field_names,
            } => {
                if let Some(name) = name {
                    write!(f, "struct {} {{ ", name)?;
                } else {
                    f.write_str("struct { ")?;
                }
                for (index, &field) in field_types.iter().enumerate() {
                    let suffix = if index + 1 == field_types.len() {
                        ""
                    } else {
                        ", "
                    };
                    if let Some(field_names) = field_names {
                        write!(f, "{}: ", field_names[index])?;
                    }
                    write!(f, "{}{}", self.cx.debug_type(field), suffix)?;
                }
                f.write_str(" }")
            }
            SpirvType::Vector { element, count } => {
                let elem = self.cx.debug_type(element);
                let len = self.cx.builder.lookup_const_u64(count);
                let len = len.expect("Vector type has invalid count value");
                write!(f, "vec<{}, {}>", elem, len)
            }
            SpirvType::Array { element, count } => {
                let elem = self.cx.debug_type(element);
                let len = self.cx.builder.lookup_const_u64(count);
                let len = len.expect("Array type has invalid count value");
                write!(f, "[{}; {}]", elem, len)
            }
            SpirvType::Pointer {
                storage_class,
                pointee,
            } => {
                let pointee = self.cx.debug_type(pointee);
                write!(f, "*{{{:?}}} {}", storage_class, pointee)
            }
            SpirvType::Function {
                return_type,
                ref arguments,
            } => {
                f.write_str("fn(")?;
                for (index, &arg) in arguments.iter().enumerate() {
                    let suffix = if index + 1 == arguments.len() {
                        ""
                    } else {
                        ", "
                    };
                    write!(f, "{}{}", self.cx.debug_type(arg), suffix)?;
                }
                let ret_type = self.cx.debug_type(return_type);
                write!(f, ") -> {}", ret_type)
            }
        }
    }
}

// returns (function_type, return_type, argument_types)
pub fn trans_fnabi<'spv, 'tcx>(
    cx: &CodegenCx<'spv, 'tcx>,
    fn_abi: &FnAbi<'tcx, Ty<'tcx>>,
) -> (Word, Word, Vec<Word>) {
    let mut argument_types = Vec::new();

    let return_type = match fn_abi.ret.mode {
        PassMode::Ignore => SpirvType::Void.def(cx),
        PassMode::Direct(_arg_attributes) => trans_type_immediate(cx, fn_abi.ret.layout),
        PassMode::Pair(_arg_attributes_1, _arg_attributes_2) => trans_type(cx, fn_abi.ret.layout),
        // TODO: Is this right?
        PassMode::Cast(_cast_target) => trans_type(cx, fn_abi.ret.layout),
        // TODO: Deal with wide ptr?
        PassMode::Indirect(_arg_attributes, _wide_ptr_attrs) => {
            let pointee = trans_type(cx, fn_abi.ret.layout);
            let pointer = SpirvType::Pointer {
                storage_class: StorageClass::Generic,
                pointee,
            }
            .def(cx);
            // Important: the return pointer comes *first*, not last.
            argument_types.push(pointer);
            SpirvType::Void.def(cx)
        }
    };

    for arg in &fn_abi.args {
        let arg_type = match arg.mode {
            PassMode::Ignore => panic!(
                "TODO: Argument PassMode::Ignore not supported yet: {:?}",
                arg
            ),
            PassMode::Direct(_arg_attributes) => trans_type_immediate(cx, arg.layout),
            PassMode::Pair(_arg_attributes_1, _arg_attributes_2) => {
                // TODO: Make this more efficient, don't generate struct
                let tuple = cx.lookup_type(trans_type(cx, arg.layout));
                let (left, right) = match tuple {
                    SpirvType::Adt {
                        ref field_types, ..
                    } => {
                        if let [left, right] = *field_types.as_slice() {
                            (left, right)
                        } else {
                            panic!("PassMode::Pair did not produce tuple: {:?}", tuple)
                        }
                    }
                    _ => panic!("PassMode::Pair did not produce tuple: {:?}", tuple),
                };
                argument_types.push(left);
                argument_types.push(right);
                continue;
            }
            PassMode::Cast(_cast_target) => trans_type(cx, arg.layout),
            // TODO: Deal with wide ptr?
            PassMode::Indirect(_arg_attributes, _wide_ptr_attrs) => {
                let pointee = trans_type(cx, arg.layout);
                SpirvType::Pointer {
                    storage_class: StorageClass::Generic,
                    pointee,
                }
                .def(cx)
            }
        };
        argument_types.push(arg_type);
    }

    let function_type = SpirvType::Function {
        return_type,
        arguments: argument_types.clone(),
    }
    .def(cx);
    (function_type, return_type, argument_types)
}

pub fn trans_type_immediate<'spv, 'tcx>(cx: &CodegenCx<'spv, 'tcx>, ty: TyAndLayout<'tcx>) -> Word {
    trans_type_impl(cx, ty, true)
}

pub fn trans_type<'spv, 'tcx>(cx: &CodegenCx<'spv, 'tcx>, ty: TyAndLayout<'tcx>) -> Word {
    trans_type_impl(cx, ty, false)
}

fn trans_type_impl<'spv, 'tcx>(
    cx: &CodegenCx<'spv, 'tcx>,
    ty: TyAndLayout<'tcx>,
    is_immediate: bool,
) -> Word {
    if ty.is_zst() {
        // An empty struct is zero-sized
        return SpirvType::Adt {
            name: None,
            field_types: Vec::new(),
            field_offsets: None,
            field_names: None,
        }
        .def(cx);
    }

    // Note: ty.abi is orthogonal to ty.variants and ty.fields, e.g. `ManuallyDrop<Result<isize, isize>>`
    // has abi `ScalarPair`.
    match ty.abi {
        Abi::Uninhabited => panic!(
            "TODO: Abi::Uninhabited not supported yet in trans_type: {:?}",
            ty
        ),
        Abi::Scalar(ref scalar) => trans_scalar_known_ty(cx, ty, scalar, is_immediate),
        Abi::ScalarPair(ref one, ref two) => {
            let one_spirv = trans_scalar_pair(cx, ty, one, 0, is_immediate);
            let two_spirv = trans_scalar_pair(cx, ty, two, 1, is_immediate);
            SpirvType::Adt {
                name: Some(format!("{}", ty.ty)),
                field_types: vec![one_spirv, two_spirv],
                field_offsets: None,
                field_names: None,
            }
            .def(cx)
        }
        Abi::Vector { ref element, count } => {
            let elem_spirv = trans_scalar_known_ty(cx, ty, element, is_immediate);
            SpirvType::Vector {
                element: elem_spirv,
                count: count as u32,
            }
            .def(cx)
        }
        Abi::Aggregate { sized: _ } => trans_aggregate(cx, ty),
    }
}

fn trans_scalar_known_ty<'spv, 'tcx>(
    cx: &CodegenCx<'spv, 'tcx>,
    ty: TyAndLayout<'tcx>,
    scalar: &Scalar,
    is_immediate: bool,
) -> Word {
    // When we know the ty, try to fill in the pointer type in case we have it, instead of defaulting to pointer to u8.
    if scalar.value == Primitive::Pointer {
        match ty.ty.kind {
            TyKind::Ref(_region, ty, _mutability) => {
                let pointee = trans_type(cx, cx.layout_of(ty));
                return SpirvType::Pointer {
                    storage_class: StorageClass::Generic,
                    pointee,
                }
                .def(cx);
            }
            TyKind::RawPtr(type_and_mut) => {
                let pointee = trans_type(cx, cx.layout_of(type_and_mut.ty));
                return SpirvType::Pointer {
                    storage_class: StorageClass::Generic,
                    pointee,
                }
                .def(cx);
            }
            TyKind::Adt(def, _) if def.is_box() => {
                let ptr_ty = cx.layout_of(cx.tcx.mk_mut_ptr(ty.ty.boxed_ty()));
                return trans_type(cx, ptr_ty);
            }
            TyKind::Adt(_adt, _substs) => {}
            // TODO: Do we fall back on trans_scalar on every weird TyKind?
            ref kind => panic!(
                "TODO: Unimplemented Primitive::Pointer TyKind ({:#?}):\n{:#?}",
                kind, ty
            ),
        }
    }

    // fall back
    trans_scalar_generic(cx, scalar, is_immediate)
}

fn trans_scalar_pair<'spv, 'tcx>(
    cx: &CodegenCx<'spv, 'tcx>,
    ty: TyAndLayout<'tcx>,
    scalar: &Scalar,
    index: usize,
    is_immediate: bool,
) -> Word {
    match ty.ty.kind {
        TyKind::Ref(..) | TyKind::RawPtr(_) => {
            return trans_type(cx, ty.field(cx, index));
        }
        TyKind::Adt(def, _) if def.is_box() => {
            let ptr_ty = cx.layout_of(cx.tcx.mk_mut_ptr(ty.ty.boxed_ty()));
            return trans_scalar_pair(cx, ptr_ty, scalar, index, is_immediate);
        }
        TyKind::Adt(_adt, _substs) => {}
        // TODO: Do we fall back on trans_scalar on every weird TyKind?
        ref kind => panic!(
            "TODO: Unimplemented Primitive::Pointer TyKind ({:#?}):\n{:#?}",
            kind, ty
        ),
    }
    trans_scalar_generic(cx, scalar, is_immediate)
}

fn trans_scalar_generic<'spv, 'tcx>(
    cx: &CodegenCx<'spv, 'tcx>,
    scalar: &Scalar,
    is_immediate: bool,
) -> Word {
    if is_immediate && scalar.is_bool() {
        return SpirvType::Bool.def(cx);
    }

    match scalar.value {
        // TODO: Do we use scalar.valid_range?
        Primitive::Int(width, signedness) => {
            SpirvType::Integer(width.size().bits() as u32, signedness).def(cx)
        }
        Primitive::F32 => SpirvType::Float(32).def(cx),
        Primitive::F64 => SpirvType::Float(64).def(cx),
        Primitive::Pointer => {
            // It is extremely difficult for us to figure out the underlying scalar type here - rustc is not
            // designed for this. For example, codegen_llvm emits a pointer to i8 here, in the method
            // scalar_llvm_type_at, called from scalar_pair_element_llvm_type. The pointer is then bitcasted to
            // the right type at the use site.
            SpirvType::Pointer {
                storage_class: StorageClass::Generic,
                pointee: SpirvType::Integer(8, false).def(cx),
            }
            .def(cx)
        }
    }
}

fn trans_aggregate<'spv, 'tcx>(cx: &CodegenCx<'spv, 'tcx>, ty: TyAndLayout<'tcx>) -> Word {
    match ty.fields {
        FieldsShape::Primitive => panic!(
            "FieldsShape::Primitive not supported yet in trans_type: {:?}",
            ty
        ),
        // TODO: Is this the right thing to do?
        FieldsShape::Union(_field_count) => {
            assert_ne!(ty.size.bytes(), 0);
            let byte = SpirvType::Integer(8, false).def(cx);
            let count = cx.constant_u32(ty.size.bytes() as u32).def;
            SpirvType::Array {
                element: byte,
                count,
            }
            .def(cx)
        }
        FieldsShape::Array { stride: _, count } => {
            // spir-v doesn't support zero-sized arrays
            // note that zero-sized arrays don't report as .is_zst() for some reason? TODO: investigate why
            let nonzero_count = if count == 0 { 1 } else { count };
            // TODO: Assert stride is same as spirv's stride?
            let element_type = trans_type(cx, ty.field(cx, 0));
            let count_const = cx.constant_u32(nonzero_count as u32).def;
            SpirvType::Array {
                element: element_type,
                count: count_const,
            }
            .def(cx)
        }
        FieldsShape::Arbitrary {
            offsets: _,
            memory_index: _,
        } => trans_struct(cx, ty),
    }
}

// see struct_llfields in librustc_codegen_llvm for implementation hints
fn trans_struct<'spv, 'tcx>(cx: &CodegenCx<'spv, 'tcx>, ty: TyAndLayout<'tcx>) -> Word {
    // TODO: enums
    let (adt, substs) = match &ty.ty.kind {
        TyKind::Adt(adt, substs) => (adt, substs),
        // "An unsized FFI type that is opaque to Rust"
        TyKind::Foreign(_def_id) => return SpirvType::Void.def(cx),
        other => panic!("TODO: Unimplemented TyKind in trans_struct: {:?}", other),
    };
    let variant = match ty.variants {
        Variants::Single { index } => &adt.variants[index],
        Variants::Multiple { .. } => panic!("Variants::Multiple not supported in trans_struct yet"),
    };
    let name = variant.ident.name;
    let mut field_types = Vec::new();
    let mut field_offsets = Vec::new();
    let mut field_names = Vec::new();
    for i in ty.fields.index_by_increasing_offset() {
        let field = &variant.fields[i];
        let field_ty = cx.layout_of(field.ty(cx.tcx, substs));
        field_types.push(trans_type(cx, field_ty));
        let offset = ty.fields.offset(i).bytes();
        field_offsets.push(offset as u32);
        field_names.push(field.ident.name.to_ident_string());
    }
    SpirvType::Adt {
        name: Some(name.to_ident_string()),
        field_types,
        field_offsets: Some(field_offsets),
        field_names: Some(field_names),
    }
    .def(cx)
}