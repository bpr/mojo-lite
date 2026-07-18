//! Canonical checked representation for origins and reference permissions.
//!
//! Source origin clauses are expressions because the parser must preserve
//! syntax it cannot resolve. Past the checker boundary, origins use stable
//! binding identities and deliberately lose source names.

use std::cmp::Ordering;

/// Stable identity of a value binding in one checker run.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct OwnerId(pub u32);

/// Stable identity of a declared or inferred origin parameter.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct OriginParamId(pub u32);

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
/// One projection step within an origin-tracked place.
pub enum OriginSeg {
    /// A named struct field.
    Field(String),
    /// An index whose value is not part of the static origin identity.
    AnyIndex,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
/// Stable owner plus its statically known projection path.
pub struct OriginPlace {
    pub root: OwnerId,
    pub path: Vec<OriginSeg>,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
/// Canonical checked description of storage a reference may designate.
pub enum Origin {
    Param(OriginParamId),
    Place(OriginPlace),
    Union(Vec<Origin>),
    Static,
    Untracked { mutable: bool },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
/// Permission available through a checked reference.
pub enum Mutability {
    Immutable,
    Mutable,
    Param(OriginParamId),
}

/// Provenance retained by an origin-bearing unsafe pointer type.  `Legacy`
/// represents Mojito's one-argument compatibility spelling; all current-Mojo
/// spellings remain explicit through checked HIR/MIR and are erased only by the
/// VM value representation.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum PointerOrigin {
    Legacy,
    Place {
        place: OriginPlace,
        mutable: bool,
    },
    Param {
        id: OriginParamId,
        mutability: Mutability,
    },
    Static,
    Untracked {
        mutable: bool,
    },
    UnsafeAny {
        mutable: bool,
    },
}

impl PointerOrigin {
    /// The loan-tracked [`Origin`] a pointer provenance corresponds to, when it
    /// designates checked storage. `Legacy`, `Static`, `Untracked`, and
    /// `UnsafeAny` pointers carry no owner loan.
    pub fn as_origin(&self) -> Option<Origin> {
        match self {
            PointerOrigin::Place { place, .. } => Some(Origin::Place(place.clone())),
            PointerOrigin::Param { id, .. } => Some(Origin::Param(*id)),
            PointerOrigin::Legacy
            | PointerOrigin::Static
            | PointerOrigin::Untracked { .. }
            | PointerOrigin::UnsafeAny { .. } => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
/// Checked reference type: referent, storage origin, and access permission.
pub struct RefTy {
    pub referent: Box<crate::types::Ty>,
    pub origin: Origin,
    pub mutability: Mutability,
}

/// An origin in a callable contract. Unlike [`Origin`], roots name parameter
/// slots rather than checker-local bindings, so the contract survives overload
/// storage and can be substituted with caller places.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SigOrigin {
    Self_,
    Param(usize),
    Static,
    Untracked { mutable: bool },
    Projected(Box<SigOrigin>, Vec<OriginSeg>),
    Union(Vec<SigOrigin>),
    Infer,
}

#[derive(Debug, Clone, PartialEq, Eq)]
/// Mutability component of a callable reference contract.
pub enum SigMutability {
    Immutable,
    Mutable,
    BoolParam(String),
    Infer,
}

#[derive(Debug, Clone, PartialEq, Eq)]
/// Reference result or parameter contract retained in a callable signature.
pub struct RefSig {
    pub origin: SigOrigin,
    pub mutability: SigMutability,
}

impl Origin {
    /// Construct a canonical union: flatten nesting, remove duplicates, and
    /// sort members independently of source order.
    pub fn union(origins: impl IntoIterator<Item = Origin>) -> Origin {
        let mut members = Vec::new();
        for origin in origins {
            match origin {
                Origin::Union(inner) => members.extend(inner),
                other => members.push(other),
            }
        }
        members.sort_by(cmp_origin);
        members.dedup();
        match members.len() {
            0 => Origin::Union(Vec::new()),
            1 => members.pop().expect("one union member"),
            _ => Origin::Union(members),
        }
    }

    /// Whether two origins may designate overlapping storage.
    pub fn overlaps(&self, other: &Origin) -> bool {
        match (self, other) {
            (Origin::Union(left), right) => left.iter().any(|item| item.overlaps(right)),
            (left, Origin::Union(right)) => right.iter().any(|item| left.overlaps(item)),
            (Origin::Place(left), Origin::Place(right)) => places_overlap(left, right),
            (Origin::Param(left), Origin::Param(right)) => left == right,
            (Origin::Static, Origin::Static) => true,
            // Untracked origins intentionally forfeit disjointness information.
            (Origin::Untracked { .. }, _) | (_, Origin::Untracked { .. }) => true,
            _ => false,
        }
    }
}

/// Whether two projected owner places may designate overlapping storage.
pub fn places_overlap(left: &OriginPlace, right: &OriginPlace) -> bool {
    if left.root != right.root {
        return false;
    }
    left.path.iter().zip(&right.path).all(|(a, b)| {
        a == b || matches!(a, OriginSeg::AnyIndex) || matches!(b, OriginSeg::AnyIndex)
    })
}

fn cmp_origin(left: &Origin, right: &Origin) -> Ordering {
    fn tag(origin: &Origin) -> u8 {
        match origin {
            Origin::Param(_) => 0,
            Origin::Place(_) => 1,
            Origin::Static => 2,
            Origin::Untracked { .. } => 3,
            Origin::Union(_) => 4,
        }
    }
    tag(left)
        .cmp(&tag(right))
        .then_with(|| match (left, right) {
            (Origin::Param(a), Origin::Param(b)) => a.cmp(b),
            (Origin::Place(a), Origin::Place(b)) => a.cmp(b),
            (Origin::Untracked { mutable: a }, Origin::Untracked { mutable: b }) => a.cmp(b),
            (Origin::Union(a), Origin::Union(b)) => a.len().cmp(&b.len()).then_with(|| {
                a.iter()
                    .zip(b)
                    .map(|(x, y)| cmp_origin(x, y))
                    .find(|ordering| !ordering.is_eq())
                    .unwrap_or(Ordering::Equal)
            }),
            _ => Ordering::Equal,
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn place(root: u32, path: &[OriginSeg]) -> Origin {
        Origin::Place(OriginPlace {
            root: OwnerId(root),
            path: path.to_vec(),
        })
    }

    #[test]
    fn unions_are_flattened_deduplicated_and_ordered() {
        let a = place(1, &[]);
        let b = place(2, &[]);
        assert_eq!(
            Origin::union([b.clone(), Origin::union([a.clone(), b])]),
            Origin::Union(vec![a, place(2, &[])])
        );
    }

    #[test]
    fn projected_places_use_prefix_and_wildcard_overlap() {
        let field_a = OriginSeg::Field("a".into());
        let field_b = OriginSeg::Field("b".into());
        assert!(place(1, &[]).overlaps(&place(1, std::slice::from_ref(&field_a))));
        assert!(!place(1, std::slice::from_ref(&field_a)).overlaps(&place(1, &[field_b])));
        assert!(place(1, &[OriginSeg::AnyIndex]).overlaps(&place(1, &[field_a])));
        assert!(!place(1, &[]).overlaps(&place(2, &[])));
    }
}
