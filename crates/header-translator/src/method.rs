use std::fmt;

use clang::{Entity, EntityKind, ObjCAttributes, ObjCQualifiers};
use tracing::span::EnteredSpan;

use crate::availability::Availability;
use crate::config::MethodData;
use crate::context::Context;
use crate::id::ItemIdentifier;
use crate::immediate_children;
use crate::objc2_utils::in_selector_family;
use crate::rust_type::{MethodArgumentQualifier, Ty};
use crate::unexposed_macro::UnexposedMacro;

impl MethodArgumentQualifier {
    pub fn parse(qualifiers: ObjCQualifiers) -> Self {
        match qualifiers {
            ObjCQualifiers {
                in_: true,
                inout: false,
                out: false,
                bycopy: false,
                byref: false,
                oneway: false,
            } => Self::In,
            ObjCQualifiers {
                in_: false,
                inout: true,
                out: false,
                bycopy: false,
                byref: false,
                oneway: false,
            } => Self::Inout,
            ObjCQualifiers {
                in_: false,
                inout: false,
                out: true,
                bycopy: false,
                byref: false,
                oneway: false,
            } => Self::Out,
            qualifiers => unreachable!("unsupported qualifiers {qualifiers:?}"),
        }
    }
}

#[derive(Debug, PartialEq, Eq, Hash, Clone, Copy)]
pub enum MemoryManagement {
    /// Consumes self and returns retained pointer
    Init,
    ReturnsRetained,
    ReturnsInnerPointer,
    Normal,
}

impl MemoryManagement {
    /// Verifies that the selector and the memory management rules match up
    /// in a way that we can just use `msg_send_id!`.
    fn verify_sel(self, sel: &str) {
        let bytes = sel.as_bytes();
        if in_selector_family(bytes, b"init") {
            assert!(self == Self::Init, "{self:?} did not match {sel}");
        } else if in_selector_family(bytes, b"new")
            || in_selector_family(bytes, b"alloc")
            || in_selector_family(bytes, b"copy")
            || in_selector_family(bytes, b"mutableCopy")
        {
            assert!(
                self == Self::ReturnsRetained,
                "{self:?} did not match {sel}"
            );
        } else {
            if self == Self::ReturnsInnerPointer {
                return;
            }
            assert!(self == Self::Normal, "{self:?} did not match {sel}");
        }
    }

    /// Matches `objc2::__macro_helpers::retain_semantics`.
    fn get_memory_management_name(sel: &str) -> &'static str {
        let bytes = sel.as_bytes();
        match (
            in_selector_family(bytes, b"new"),
            in_selector_family(bytes, b"alloc"),
            in_selector_family(bytes, b"init"),
            in_selector_family(bytes, b"copy"),
            in_selector_family(bytes, b"mutableCopy"),
        ) {
            (true, false, false, false, false) => "New",
            (false, true, false, false, false) => "Alloc",
            (false, false, true, false, false) => "Init",
            (false, false, false, true, false) => "CopyOrMutCopy",
            (false, false, false, false, true) => "CopyOrMutCopy",
            (false, false, false, false, false) => "Other",
            _ => unreachable!(),
        }
    }

    pub fn is_init(sel: &str) -> bool {
        in_selector_family(sel.as_bytes(), b"init")
    }

    pub fn is_alloc(sel: &str) -> bool {
        in_selector_family(sel.as_bytes(), b"alloc")
    }

    pub fn is_new(sel: &str) -> bool {
        in_selector_family(sel.as_bytes(), b"new")
    }
}

#[allow(dead_code)]
#[derive(Debug, Clone, PartialEq)]
pub struct Method {
    selector: String,
    pub fn_name: String,
    availability: Availability,
    pub is_class: bool,
    is_optional_protocol: bool,
    memory_management: MemoryManagement,
    arguments: Vec<(String, Ty)>,
    pub result_type: Ty,
    safe: bool,
    mutating: bool,
}

impl Method {
    /// Value that uniquely identifies the method in a class.
    pub fn id(&self) -> (bool, &str) {
        (self.is_class, &self.selector)
    }

    /// Takes one of `EntityKind::ObjCInstanceMethodDecl` or
    /// `EntityKind::ObjCClassMethodDecl`.
    pub fn partial(entity: Entity<'_>) -> PartialMethod<'_> {
        let selector = entity.get_name().expect("method selector");
        let fn_name = selector.trim_end_matches(|c| c == ':').replace(':', "_");

        let _span = debug_span!("method", fn_name).entered();

        let is_class = match entity.get_kind() {
            EntityKind::ObjCInstanceMethodDecl => false,
            EntityKind::ObjCClassMethodDecl => true,
            _ => unreachable!("unknown method kind"),
        };

        PartialMethod {
            entity,
            selector,
            is_class,
            fn_name,
            _span,
        }
    }

    /// Takes `EntityKind::ObjCPropertyDecl`.
    pub fn partial_property(entity: Entity<'_>) -> PartialProperty<'_> {
        let attributes = entity.get_objc_attributes();
        let has_setter = attributes.map(|a| !a.readonly).unwrap_or(true);

        let name = entity.get_display_name().expect("property name");
        let _span = debug_span!("property", name).entered();

        PartialProperty {
            entity,
            name,
            getter_name: entity.get_objc_getter_name().expect("property getter name"),
            setter_name: has_setter.then(|| {
                entity
                    .get_objc_setter_name()
                    .expect("property setter name")
                    .trim_end_matches(|c| c == ':')
                    .to_string()
            }),
            is_class: attributes.map(|a| a.class).unwrap_or(false),
            attributes,
            _span,
        }
    }

    pub fn update(mut self, data: MethodData) -> Option<Self> {
        if data.skipped {
            return None;
        }

        self.mutating = data.mutating;
        self.safe = !data.unsafe_;

        Some(self)
    }

    pub fn visit_required_types(&self, mut f: impl FnMut(&ItemIdentifier)) {
        for (_, arg) in &self.arguments {
            arg.visit_required_types(&mut f);
        }

        self.result_type.visit_required_types(&mut f);
    }
}

#[derive(Debug)]
pub struct PartialMethod<'tu> {
    entity: Entity<'tu>,
    selector: String,
    pub is_class: bool,
    pub fn_name: String,
    _span: EnteredSpan,
}

impl<'tu> PartialMethod<'tu> {
    pub fn parse(self, data: MethodData, context: &Context<'_>) -> Option<(bool, Method)> {
        let Self {
            entity,
            selector,
            is_class,
            fn_name,
            _span,
        } = self;

        if data.skipped {
            return None;
        }

        if entity.is_variadic() {
            warn!("can't handle variadic method");
            return None;
        }

        let availability = Availability::parse(
            entity
                .get_platform_availability()
                .expect("method availability"),
        );

        let mut arguments: Vec<_> = entity
            .get_arguments()
            .expect("method arguments")
            .into_iter()
            .map(|entity| {
                let name = entity.get_name().expect("arg display name");
                let _span = debug_span!("method argument", name).entered();
                let qualifier = entity
                    .get_objc_qualifiers()
                    .map(MethodArgumentQualifier::parse);

                immediate_children(&entity, |entity, _span| match entity.get_kind() {
                    EntityKind::ObjCClassRef
                    | EntityKind::ObjCProtocolRef
                    | EntityKind::TypeRef
                    | EntityKind::ParmDecl => {
                        // Ignore
                    }
                    EntityKind::NSConsumed => {
                        error!("found NSConsumed, which requires manual handling");
                    }
                    EntityKind::UnexposedAttr => {
                        if let Some(macro_) = UnexposedMacro::parse(&entity) {
                            warn!(?macro_, "unknown macro");
                        }
                    }
                    // For some reason we recurse into array types
                    EntityKind::IntegerLiteral => {}
                    _ => error!("unknown"),
                });

                let ty = entity.get_type().expect("argument type");
                let ty = Ty::parse_method_argument(ty, qualifier, context);

                (name, ty)
            })
            .collect();

        let is_error = if let Some((_, ty)) = arguments.last() {
            ty.argument_is_error_out()
        } else {
            false
        };

        // TODO: Strip these from function name?
        // selector.ends_with("error:")
        // || selector.ends_with("AndReturnError:")
        // || selector.ends_with("WithError:")

        if is_error {
            arguments.pop();
        }

        if let Some(qualifiers) = entity.get_objc_qualifiers() {
            error!(?qualifiers, "unsupported qualifiers on return type");
        }

        let result_type = entity.get_result_type().expect("method return type");
        let mut result_type = Ty::parse_method_return(result_type, context);

        result_type.fix_related_result_type(is_class, &selector);

        if is_class && MemoryManagement::is_alloc(&selector) {
            result_type.set_is_alloc();
        }

        if is_error {
            result_type.set_is_error();
        }

        let mut designated_initializer = false;
        let mut consumes_self = false;
        let mut memory_management = MemoryManagement::Normal;

        immediate_children(&entity, |entity, _span| match entity.get_kind() {
            EntityKind::ObjCClassRef
            | EntityKind::ObjCProtocolRef
            | EntityKind::TypeRef
            | EntityKind::ParmDecl => {
                // Ignore
            }
            EntityKind::ObjCDesignatedInitializer => {
                if designated_initializer {
                    error!("encountered ObjCDesignatedInitializer twice");
                }
                designated_initializer = true;
            }
            EntityKind::NSConsumesSelf => {
                consumes_self = true;
            }
            EntityKind::NSReturnsAutoreleased => {
                error!("found NSReturnsAutoreleased, which requires manual handling");
            }
            EntityKind::NSReturnsRetained => {
                if memory_management != MemoryManagement::Normal {
                    error!("got unexpected NSReturnsRetained")
                }
                memory_management = MemoryManagement::ReturnsRetained;
            }
            EntityKind::NSReturnsNotRetained => {
                error!("found NSReturnsNotRetained, which is not yet supported");
            }
            EntityKind::ObjCReturnsInnerPointer => {
                if memory_management != MemoryManagement::Normal {
                    error!("got unexpected ObjCReturnsInnerPointer")
                }
                memory_management = MemoryManagement::ReturnsInnerPointer;
            }
            EntityKind::IbActionAttr => {
                // TODO: What is this?
            }
            EntityKind::ObjCRequiresSuper => {
                // TODO: Can we use this for something?
                // <https://clang.llvm.org/docs/AttributeReference.html#objc-requires-super>
            }
            EntityKind::WarnUnusedResultAttr => {
                // TODO: Emit `#[must_use]` on this
            }
            EntityKind::UnexposedAttr => {
                if let Some(macro_) = UnexposedMacro::parse(&entity) {
                    warn!(?macro_, "unknown macro");
                }
            }
            _ => error!("unknown"),
        });

        if consumes_self {
            if memory_management != MemoryManagement::ReturnsRetained {
                error!("got NSConsumesSelf without NSReturnsRetained");
            }
            memory_management = MemoryManagement::Init;
        }

        // Verify that memory management is as expected
        if result_type.is_id() {
            memory_management.verify_sel(&selector);
        }

        if data.mutating && (is_class || MemoryManagement::is_init(&selector)) {
            error!("invalid mutating method");
        }

        Some((
            designated_initializer,
            Method {
                selector,
                fn_name,
                availability,
                is_class,
                is_optional_protocol: entity.is_objc_optional(),
                memory_management,
                arguments,
                result_type,
                safe: !data.unsafe_,
                mutating: data.mutating,
            },
        ))
    }
}

#[derive(Debug)]
pub struct PartialProperty<'tu> {
    pub entity: Entity<'tu>,
    pub name: String,
    pub getter_name: String,
    pub setter_name: Option<String>,
    pub is_class: bool,
    pub attributes: Option<ObjCAttributes>,
    pub _span: EnteredSpan,
}

impl PartialProperty<'_> {
    pub fn parse(
        self,
        getter_data: MethodData,
        setter_data: Option<MethodData>,
        context: &Context<'_>,
    ) -> (Option<Method>, Option<Method>) {
        let Self {
            entity,
            name,
            getter_name,
            setter_name,
            is_class,
            attributes,
            _span,
        } = self;

        // Early return if both getter and setter are skipped
        //
        // To reduce warnings.
        if getter_data.skipped && setter_data.map(|data| data.skipped).unwrap_or(true) {
            return (None, None);
        }

        let availability = Availability::parse(
            entity
                .get_platform_availability()
                .expect("method availability"),
        );

        let is_copy = attributes.map(|a| a.copy).unwrap_or(false);

        let mut memory_management = MemoryManagement::Normal;

        immediate_children(&entity, |entity, _span| match entity.get_kind() {
            EntityKind::ObjCClassRef
            | EntityKind::ObjCProtocolRef
            | EntityKind::TypeRef
            | EntityKind::ParmDecl => {
                // Ignore
            }
            EntityKind::ObjCReturnsInnerPointer => {
                if memory_management != MemoryManagement::Normal {
                    error!(?memory_management, "unexpected ObjCReturnsInnerPointer")
                }
                memory_management = MemoryManagement::ReturnsInnerPointer;
            }
            EntityKind::ObjCInstanceMethodDecl => {
                warn!("method in property");
            }
            EntityKind::IbOutletAttr => {
                // TODO: What is this?
            }
            EntityKind::UnexposedAttr => {
                if let Some(macro_) = UnexposedMacro::parse(&entity) {
                    warn!(?macro_, "unknown macro");
                }
            }
            _ => error!("unknown"),
        });

        if let Some(qualifiers) = entity.get_objc_qualifiers() {
            error!(?qualifiers, "properties do not support qualifiers");
        }

        let getter = if !getter_data.skipped {
            let ty = Ty::parse_property_return(
                entity.get_type().expect("property type"),
                is_copy,
                context,
            );

            Some(Method {
                selector: getter_name.clone(),
                fn_name: getter_name,
                availability: availability.clone(),
                is_class,
                is_optional_protocol: entity.is_objc_optional(),
                memory_management,
                arguments: Vec::new(),
                result_type: ty,
                safe: !getter_data.unsafe_,
                mutating: getter_data.mutating,
            })
        } else {
            None
        };

        let setter = if let Some(setter_name) = setter_name {
            let setter_data = setter_data.expect("setter_data must be present if setter_name was");
            if !setter_data.skipped {
                let ty =
                    Ty::parse_property(entity.get_type().expect("property type"), is_copy, context);

                Some(Method {
                    selector: setter_name.clone() + ":",
                    fn_name: setter_name,
                    availability,
                    is_class,
                    is_optional_protocol: entity.is_objc_optional(),
                    memory_management,
                    arguments: vec![(name, ty)],
                    result_type: Ty::VOID_RESULT,
                    safe: !setter_data.unsafe_,
                    mutating: setter_data.mutating,
                })
            } else {
                None
            }
        } else {
            None
        };

        (getter, setter)
    }
}

impl Method {
    pub(crate) fn emit_on_subclasses(&self) -> bool {
        if !self.result_type.is_instancetype() {
            return false;
        }
        if self.is_class {
            !matches!(&*self.selector, "new" | "supportsSecureCoding")
        } else {
            self.memory_management == MemoryManagement::Init
                && !matches!(&*self.selector, "init" | "initWithCoder:")
        }
    }
}

impl fmt::Display for Method {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let _span = debug_span!("method", self.fn_name).entered();

        if self.is_optional_protocol {
            writeln!(f, "        #[optional]")?;
        }

        let error_trailing = if self.result_type.is_error() { "_" } else { "" };

        if self.result_type.is_id() {
            writeln!(
                f,
                "        #[method_id(@__retain_semantics {} {}{})]",
                MemoryManagement::get_memory_management_name(&self.selector),
                self.selector,
                error_trailing,
            )?;
        } else {
            writeln!(f, "        #[method({}{})]", self.selector, error_trailing)?;
        };

        write!(f, "        pub ")?;
        if !self.safe {
            write!(f, "unsafe ")?;
        }
        write!(f, "fn {}(", handle_reserved(&self.fn_name))?;
        if !self.is_class {
            if MemoryManagement::is_init(&self.selector) {
                write!(f, "this: Option<Allocated<Self>>, ")?;
            } else if self.mutating {
                write!(f, "&mut self, ")?;
            } else {
                write!(f, "&self, ")?;
            }
        }
        for (param, arg_ty) in &self.arguments {
            write!(f, "{}: {arg_ty},", handle_reserved(param))?;
        }
        write!(f, ")")?;

        writeln!(f, "{};", self.result_type)?;

        Ok(())
    }
}

pub(crate) fn handle_reserved(s: &str) -> &str {
    match s {
        "type" => "type_",
        "trait" => "trait_",
        "abstract" => "abstract_",
        s => s,
    }
}
