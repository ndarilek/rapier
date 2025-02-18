use crate::data::{BundleSet, ComponentSet, ComponentSetOption};
use crate::dynamics::{
    IslandManager, RigidBodyColliders, RigidBodyForces, RigidBodyMassProps, RigidBodyPosition,
    RigidBodyVelocity,
};
use crate::geometry::{
    ColliderFlags, ColliderHandle, ColliderParent, ColliderPosition, ColliderShape,
    InteractionGroups, PointProjection, Ray, RayIntersection, AABB, QBVH,
};
use crate::math::{Isometry, Point, Real, Vector};
use parry::partitioning::QBVHDataGenerator;
use parry::query::details::{
    IntersectionCompositeShapeShapeBestFirstVisitor,
    NonlinearTOICompositeShapeShapeBestFirstVisitor, PointCompositeShapeProjBestFirstVisitor,
    PointCompositeShapeProjWithFeatureBestFirstVisitor,
    RayCompositeShapeToiAndNormalBestFirstVisitor, RayCompositeShapeToiBestFirstVisitor,
    TOICompositeShapeShapeBestFirstVisitor,
};
use parry::query::visitors::{
    BoundingVolumeIntersectionsVisitor, PointIntersectionsVisitor, RayIntersectionsVisitor,
};
use parry::query::{DefaultQueryDispatcher, NonlinearRigidMotion, QueryDispatcher, TOI};
use parry::shape::{FeatureId, Shape, TypedSimdCompositeShape};
use std::sync::Arc;

#[cfg(feature = "default-sets")]
use crate::{dynamics::RigidBodySet, geometry::ColliderSet};

/// A pipeline for performing queries on all the colliders of a scene.
#[cfg_attr(feature = "serde-serialize", derive(Serialize, Deserialize))]
#[derive(Clone)]
pub struct QueryPipeline {
    #[cfg_attr(
        feature = "serde-serialize",
        serde(skip, default = "crate::geometry::default_query_dispatcher")
    )]
    query_dispatcher: Arc<dyn QueryDispatcher>,
    qbvh: QBVH<ColliderHandle>,
    tree_built: bool,
    dilation_factor: Real,
}

struct QueryPipelineAsCompositeShape<'a, Colliders> {
    query_pipeline: &'a QueryPipeline,
    colliders: &'a Colliders,
    query_groups: InteractionGroups,
    filter: Option<&'a dyn Fn(ColliderHandle) -> bool>,
}

/// Indicates how the colliders position should be taken into account when
/// updating the query pipeline.
pub enum QueryPipelineMode {
    /// The `Collider::position` is taken into account.
    CurrentPosition,
    /// The `RigidBody::next_position * Collider::position_wrt_parent` is taken into account for
    /// the colliders positions.
    SweepTestWithNextPosition,
    /// The `RigidBody::predict_position_using_velocity_and_forces * Collider::position_wrt_parent`
    /// is taken into account for the colliders position.
    SweepTestWithPredictedPosition {
        /// The time used to integrate the rigid-body's velocity and acceleration.
        dt: Real,
    },
}

impl<'a, Colliders> TypedSimdCompositeShape for QueryPipelineAsCompositeShape<'a, Colliders>
where
    // TODO ECS: make everything optional but the shape?
    Colliders:
        ComponentSet<ColliderFlags> + ComponentSet<ColliderPosition> + ComponentSet<ColliderShape>,
{
    type PartShape = dyn Shape;
    type PartId = ColliderHandle;

    fn map_typed_part_at(
        &self,
        shape_id: Self::PartId,
        mut f: impl FnMut(Option<&Isometry<Real>>, &Self::PartShape),
    ) {
        let co_flags: Option<&ColliderFlags> = self.colliders.get(shape_id.0);

        if let Some(co_flags) = co_flags {
            if co_flags.collision_groups.test(self.query_groups)
                && self.filter.map(|f| f(shape_id)).unwrap_or(true)
            {
                let (co_pos, co_shape): (&ColliderPosition, &ColliderShape) =
                    self.colliders.index_bundle(shape_id.0);
                f(Some(co_pos), &**co_shape)
            }
        }
    }

    fn map_untyped_part_at(
        &self,
        shape_id: Self::PartId,
        f: impl FnMut(Option<&Isometry<Real>>, &Self::PartShape),
    ) {
        self.map_typed_part_at(shape_id, f);
    }

    fn typed_qbvh(&self) -> &QBVH<ColliderHandle> {
        &self.query_pipeline.qbvh
    }
}

impl Default for QueryPipeline {
    fn default() -> Self {
        Self::new()
    }
}

impl QueryPipeline {
    /// Initializes an empty query pipeline.
    pub fn new() -> Self {
        Self::with_query_dispatcher(DefaultQueryDispatcher)
    }

    fn as_composite_shape<'a, Colliders>(
        &'a self,
        colliders: &'a Colliders,
        query_groups: InteractionGroups,
        filter: Option<&'a dyn Fn(ColliderHandle) -> bool>,
    ) -> QueryPipelineAsCompositeShape<'a, Colliders> {
        QueryPipelineAsCompositeShape {
            query_pipeline: self,
            colliders,
            query_groups,
            filter,
        }
    }

    /// Initializes an empty query pipeline with a custom `QueryDispatcher`.
    ///
    /// Use this constructor in order to use a custom `QueryDispatcher` that is
    /// aware of your own user-defined shapes.
    pub fn with_query_dispatcher<D>(d: D) -> Self
    where
        D: 'static + QueryDispatcher,
    {
        Self {
            query_dispatcher: Arc::new(d),
            qbvh: QBVH::new(),
            tree_built: false,
            dilation_factor: 0.01,
        }
    }

    /// The query dispatcher used by this query pipeline for running scene queries.
    pub fn query_dispatcher(&self) -> &dyn QueryDispatcher {
        &*self.query_dispatcher
    }

    #[cfg(feature = "default-sets")]
    /// Update the acceleration structure on the query pipeline.
    pub fn update(
        &mut self,
        islands: &IslandManager,
        bodies: &RigidBodySet,
        colliders: &ColliderSet,
    ) {
        self.update_generic(islands, bodies, colliders);
    }

    /// Update the acceleration structure on the query pipeline.
    pub fn update_generic<Bodies, Colliders>(
        &mut self,
        islands: &IslandManager,
        bodies: &Bodies,
        colliders: &Colliders,
    ) where
        Bodies: ComponentSet<RigidBodyPosition>
            + ComponentSet<RigidBodyColliders>
            + ComponentSet<RigidBodyVelocity>
            + ComponentSet<RigidBodyMassProps>
            + ComponentSet<RigidBodyForces>,
        Colliders: ComponentSet<ColliderShape>
            + ComponentSet<ColliderPosition>
            + ComponentSetOption<ColliderParent>,
    {
        self.update_with_mode(
            islands,
            bodies,
            colliders,
            QueryPipelineMode::CurrentPosition,
        )
    }

    /// Update the acceleration structure on the query pipeline.
    pub fn update_with_mode<Bodies, Colliders>(
        &mut self,
        islands: &IslandManager,
        bodies: &Bodies,
        colliders: &Colliders,
        mode: QueryPipelineMode,
    ) where
        Bodies: ComponentSet<RigidBodyPosition>
            + ComponentSet<RigidBodyColliders>
            + ComponentSet<RigidBodyVelocity>
            + ComponentSet<RigidBodyMassProps>
            + ComponentSet<RigidBodyForces>,
        Colliders: ComponentSet<ColliderShape>
            + ComponentSet<ColliderPosition>
            + ComponentSetOption<ColliderParent>,
    {
        struct DataGenerator<'a, Bs, Cs> {
            bodies: &'a Bs,
            colliders: &'a Cs,
            mode: QueryPipelineMode,
        }

        impl<'a, Bs, Cs> QBVHDataGenerator<ColliderHandle> for DataGenerator<'a, Bs, Cs>
        where
            Bs: ComponentSet<RigidBodyPosition>
                + ComponentSet<RigidBodyMassProps>
                + ComponentSet<RigidBodyVelocity>
                + ComponentSet<RigidBodyForces>,
            Cs: ComponentSet<ColliderShape>
                + ComponentSet<ColliderPosition>
                + ComponentSetOption<ColliderParent>,
        {
            fn size_hint(&self) -> usize {
                ComponentSet::<ColliderShape>::size_hint(self.colliders)
            }

            #[inline(always)]
            fn for_each(&mut self, mut f: impl FnMut(ColliderHandle, AABB)) {
                match self.mode {
                    QueryPipelineMode::CurrentPosition => {
                        self.colliders.for_each(|h, co_shape: &ColliderShape| {
                            let co_pos: &ColliderPosition = self.colliders.index(h);
                            f(ColliderHandle(h), co_shape.compute_aabb(&co_pos))
                        })
                    }
                    QueryPipelineMode::SweepTestWithNextPosition => {
                        self.colliders.for_each(|h, co_shape: &ColliderShape| {
                            let co_parent: Option<&ColliderParent> = self.colliders.get(h);
                            let co_pos: &ColliderPosition = self.colliders.index(h);

                            if let Some(co_parent) = co_parent {
                                let rb_pos: &RigidBodyPosition =
                                    self.bodies.index(co_parent.handle.0);
                                let next_position = rb_pos.next_position * co_parent.pos_wrt_parent;

                                f(
                                    ColliderHandle(h),
                                    co_shape.compute_swept_aabb(&co_pos, &next_position),
                                )
                            } else {
                                f(ColliderHandle(h), co_shape.compute_aabb(&co_pos))
                            }
                        })
                    }
                    QueryPipelineMode::SweepTestWithPredictedPosition { dt } => {
                        self.colliders.for_each(|h, co_shape: &ColliderShape| {
                            let co_parent: Option<&ColliderParent> = self.colliders.get(h);
                            let co_pos: &ColliderPosition = self.colliders.index(h);

                            if let Some(co_parent) = co_parent {
                                let (rb_pos, vels, forces, mprops): (
                                    &RigidBodyPosition,
                                    &RigidBodyVelocity,
                                    &RigidBodyForces,
                                    &RigidBodyMassProps,
                                ) = self.bodies.index_bundle(co_parent.handle.0);
                                let predicted_pos = rb_pos
                                    .integrate_forces_and_velocities(dt, forces, vels, mprops);

                                let next_position = predicted_pos * co_parent.pos_wrt_parent;
                                f(
                                    ColliderHandle(h),
                                    co_shape.compute_swept_aabb(&co_pos, &next_position),
                                )
                            } else {
                                f(ColliderHandle(h), co_shape.compute_aabb(&co_pos))
                            }
                        })
                    }
                }
            }
        }

        if !self.tree_built {
            let generator = DataGenerator {
                bodies,
                colliders,
                mode,
            };
            self.qbvh.clear_and_rebuild(generator, self.dilation_factor);

            // FIXME: uncomment this once we handle insertion/removals properly.
            // self.tree_built = true;
            return;
        }

        for handle in islands.iter_active_bodies() {
            let body_colliders: &RigidBodyColliders = bodies.index(handle.0);
            for handle in &body_colliders.0 {
                self.qbvh.pre_update(*handle)
            }
        }

        match mode {
            QueryPipelineMode::CurrentPosition => {
                self.qbvh.update(
                    |handle| {
                        let (co_pos, co_shape): (&ColliderPosition, &ColliderShape) =
                            colliders.index_bundle(handle.0);
                        co_shape.compute_aabb(&co_pos)
                    },
                    self.dilation_factor,
                );
            }
            QueryPipelineMode::SweepTestWithNextPosition => {
                self.qbvh.update(
                    |handle| {
                        let co_parent: Option<&ColliderParent> = colliders.get(handle.0);
                        let (co_pos, co_shape): (&ColliderPosition, &ColliderShape) =
                            colliders.index_bundle(handle.0);

                        if let Some(co_parent) = co_parent {
                            let rb_pos: &RigidBodyPosition = bodies.index(co_parent.handle.0);
                            let next_position = rb_pos.next_position * co_parent.pos_wrt_parent;
                            co_shape.compute_swept_aabb(&co_pos, &next_position)
                        } else {
                            co_shape.compute_aabb(&co_pos)
                        }
                    },
                    self.dilation_factor,
                );
            }
            QueryPipelineMode::SweepTestWithPredictedPosition { dt } => {
                self.qbvh.update(
                    |handle| {
                        let co_parent: Option<&ColliderParent> = colliders.get(handle.0);
                        let (co_pos, co_shape): (&ColliderPosition, &ColliderShape) =
                            colliders.index_bundle(handle.0);

                        if let Some(co_parent) = co_parent {
                            let (rb_pos, vels, forces, mprops): (
                                &RigidBodyPosition,
                                &RigidBodyVelocity,
                                &RigidBodyForces,
                                &RigidBodyMassProps,
                            ) = bodies.index_bundle(co_parent.handle.0);

                            let predicted_pos =
                                rb_pos.integrate_forces_and_velocities(dt, forces, vels, mprops);

                            let next_position = predicted_pos * co_parent.pos_wrt_parent;
                            co_shape.compute_swept_aabb(&co_pos, &next_position)
                        } else {
                            co_shape.compute_aabb(&co_pos)
                        }
                    },
                    self.dilation_factor,
                );
            }
        }
    }

    /// Find the closest intersection between a ray and a set of collider.
    ///
    /// # Parameters
    /// * `colliders` - The set of colliders taking part in this pipeline.
    /// * `ray`: the ray to cast.
    /// * `max_toi`: the maximum time-of-impact that can be reported by this cast. This effectively
    ///   limits the length of the ray to `ray.dir.norm() * max_toi`. Use `Real::MAX` for an unbounded ray.
    /// * `solid`: if this is `true` an impact at time 0.0 (i.e. at the ray origin) is returned if
    ///            it starts inside of a shape. If this `false` then the ray will hit the shape's boundary
    ///            even if its starts inside of it.
    /// * `query_groups`: the interaction groups which will be tested against the collider's `contact_group`
    ///                   to determine if it should be taken into account by this query.
    /// * `filter`: a more fine-grained filter. A collider is taken into account by this query if
    ///             its `contact_group` is compatible with the `query_groups`, and if this `filter`
    ///             is either `None` or returns `true`.
    pub fn cast_ray<Colliders>(
        &self,
        colliders: &Colliders,
        ray: &Ray,
        max_toi: Real,
        solid: bool,
        query_groups: InteractionGroups,
        filter: Option<&dyn Fn(ColliderHandle) -> bool>,
    ) -> Option<(ColliderHandle, Real)>
    where
        Colliders: ComponentSet<ColliderFlags>
            + ComponentSet<ColliderPosition>
            + ComponentSet<ColliderShape>,
    {
        let pipeline_shape = self.as_composite_shape(colliders, query_groups, filter);
        let mut visitor =
            RayCompositeShapeToiBestFirstVisitor::new(&pipeline_shape, ray, max_toi, solid);

        self.qbvh.traverse_best_first(&mut visitor).map(|h| h.1)
    }

    /// Find the closest intersection between a ray and a set of collider.
    ///
    /// # Parameters
    /// * `colliders` - The set of colliders taking part in this pipeline.
    /// * `ray`: the ray to cast.
    /// * `max_toi`: the maximum time-of-impact that can be reported by this cast. This effectively
    ///   limits the length of the ray to `ray.dir.norm() * max_toi`. Use `Real::MAX` for an unbounded ray.
    /// * `solid`: if this is `true` an impact at time 0.0 (i.e. at the ray origin) is returned if
    ///            it starts inside of a shape. If this `false` then the ray will hit the shape's boundary
    ///            even if its starts inside of it.
    /// * `query_groups`: the interaction groups which will be tested against the collider's `contact_group`
    ///                   to determine if it should be taken into account by this query.
    /// * `filter`: a more fine-grained filter. A collider is taken into account by this query if
    ///             its `contact_group` is compatible with the `query_groups`, and if this `filter`
    ///             is either `None` or returns `true`.
    pub fn cast_ray_and_get_normal<Colliders>(
        &self,
        colliders: &Colliders,
        ray: &Ray,
        max_toi: Real,
        solid: bool,
        query_groups: InteractionGroups,
        filter: Option<&dyn Fn(ColliderHandle) -> bool>,
    ) -> Option<(ColliderHandle, RayIntersection)>
    where
        Colliders: ComponentSet<ColliderFlags>
            + ComponentSet<ColliderPosition>
            + ComponentSet<ColliderShape>,
    {
        let pipeline_shape = self.as_composite_shape(colliders, query_groups, filter);
        let mut visitor = RayCompositeShapeToiAndNormalBestFirstVisitor::new(
            &pipeline_shape,
            ray,
            max_toi,
            solid,
        );

        self.qbvh.traverse_best_first(&mut visitor).map(|h| h.1)
    }

    /// Find the all intersections between a ray and a set of collider and passes them to a callback.
    ///
    /// # Parameters
    /// * `colliders` - The set of colliders taking part in this pipeline.
    /// * `ray`: the ray to cast.
    /// * `max_toi`: the maximum time-of-impact that can be reported by this cast. This effectively
    ///   limits the length of the ray to `ray.dir.norm() * max_toi`. Use `Real::MAX` for an unbounded ray.
    /// * `solid`: if this is `true` an impact at time 0.0 (i.e. at the ray origin) is returned if
    ///            it starts inside of a shape. If this `false` then the ray will hit the shape's boundary
    ///            even if its starts inside of it.
    /// * `query_groups`: the interaction groups which will be tested against the collider's `contact_group`
    ///                   to determine if it should be taken into account by this query.
    /// * `filter`: a more fine-grained filter. A collider is taken into account by this query if
    ///             its `contact_group` is compatible with the `query_groups`, and if this `filter`
    ///             is either `None` or returns `true`.
    /// * `callback`: function executed on each collider for which a ray intersection has been found.
    ///               There is no guarantees on the order the results will be yielded. If this callback returns `false`,
    ///               this method will exit early, ignore any further raycast.
    pub fn intersections_with_ray<'a, Colliders>(
        &self,
        colliders: &'a Colliders,
        ray: &Ray,
        max_toi: Real,
        solid: bool,
        query_groups: InteractionGroups,
        filter: Option<&dyn Fn(ColliderHandle) -> bool>,
        mut callback: impl FnMut(ColliderHandle, RayIntersection) -> bool,
    ) where
        Colliders: ComponentSet<ColliderFlags>
            + ComponentSet<ColliderPosition>
            + ComponentSet<ColliderShape>,
    {
        let mut leaf_callback = &mut |handle: &ColliderHandle| {
            let co_shape: Option<&ColliderShape> = colliders.get(handle.0);
            if let Some(co_shape) = co_shape {
                let (co_flags, co_pos): (&ColliderFlags, &ColliderPosition) =
                    colliders.index_bundle(handle.0);
                if co_flags.collision_groups.test(query_groups)
                    && filter.map(|f| f(*handle)).unwrap_or(true)
                {
                    if let Some(hit) = co_shape.cast_ray_and_get_normal(co_pos, ray, max_toi, solid)
                    {
                        return callback(*handle, hit);
                    }
                }
            }

            true
        };

        let mut visitor = RayIntersectionsVisitor::new(ray, max_toi, &mut leaf_callback);
        self.qbvh.traverse_depth_first(&mut visitor);
    }

    /// Gets the handle of up to one collider intersecting the given shape.
    ///
    /// # Parameters
    /// * `colliders` - The set of colliders taking part in this pipeline.
    /// * `shape_pos` - The position of the shape used for the intersection test.
    /// * `shape` - The shape used for the intersection test.
    /// * `query_groups` - the interaction groups which will be tested against the collider's `contact_group`
    ///                   to determine if it should be taken into account by this query.
    /// * `filter` - a more fine-grained filter. A collider is taken into account by this query if
    ///             its `contact_group` is compatible with the `query_groups`, and if this `filter`
    ///             is either `None` or returns `true`.
    pub fn intersection_with_shape<Colliders>(
        &self,
        colliders: &Colliders,
        shape_pos: &Isometry<Real>,
        shape: &dyn Shape,
        query_groups: InteractionGroups,
        filter: Option<&dyn Fn(ColliderHandle) -> bool>,
    ) -> Option<ColliderHandle>
    where
        Colliders: ComponentSet<ColliderFlags>
            + ComponentSet<ColliderPosition>
            + ComponentSet<ColliderShape>,
    {
        let pipeline_shape = self.as_composite_shape(colliders, query_groups, filter);
        let mut visitor = IntersectionCompositeShapeShapeBestFirstVisitor::new(
            &*self.query_dispatcher,
            shape_pos,
            &pipeline_shape,
            shape,
        );

        self.qbvh
            .traverse_best_first(&mut visitor)
            .map(|h| (h.1 .0))
    }

    /// Find the projection of a point on the closest collider.
    ///
    /// # Parameters
    /// * `colliders` - The set of colliders taking part in this pipeline.
    /// * `point` - The point to project.
    /// * `solid` - If this is set to `true` then the collider shapes are considered to
    ///   be plain (if the point is located inside of a plain shape, its projection is the point
    ///   itself). If it is set to `false` the collider shapes are considered to be hollow
    ///   (if the point is located inside of an hollow shape, it is projected on the shape's
    ///   boundary).
    /// * `query_groups` - the interaction groups which will be tested against the collider's `contact_group`
    ///                   to determine if it should be taken into account by this query.
    /// * `filter` - a more fine-grained filter. A collider is taken into account by this query if
    ///             its `contact_group` is compatible with the `query_groups`, and if this `filter`
    ///             is either `None` or returns `true`.
    pub fn project_point<Colliders>(
        &self,
        colliders: &Colliders,
        point: &Point<Real>,
        solid: bool,
        query_groups: InteractionGroups,
        filter: Option<&dyn Fn(ColliderHandle) -> bool>,
    ) -> Option<(ColliderHandle, PointProjection)>
    where
        Colliders: ComponentSet<ColliderFlags>
            + ComponentSet<ColliderPosition>
            + ComponentSet<ColliderShape>,
    {
        let pipeline_shape = self.as_composite_shape(colliders, query_groups, filter);
        let mut visitor =
            PointCompositeShapeProjBestFirstVisitor::new(&pipeline_shape, point, solid);

        self.qbvh
            .traverse_best_first(&mut visitor)
            .map(|h| (h.1 .1, h.1 .0))
    }

    /// Find all the colliders containing the given point.
    ///
    /// # Parameters
    /// * `colliders` - The set of colliders taking part in this pipeline.
    /// * `point` - The point used for the containment test.
    /// * `query_groups` - the interaction groups which will be tested against the collider's `contact_group`
    ///                   to determine if it should be taken into account by this query.
    /// * `filter` - a more fine-grained filter. A collider is taken into account by this query if
    ///             its `contact_group` is compatible with the `query_groups`, and if this `filter`
    ///             is either `None` or returns `true`.
    /// * `callback` - A function called with each collider with a shape
    ///                containing the `point`.
    pub fn intersections_with_point<'a, Colliders>(
        &self,
        colliders: &'a Colliders,
        point: &Point<Real>,
        query_groups: InteractionGroups,
        filter: Option<&dyn Fn(ColliderHandle) -> bool>,
        mut callback: impl FnMut(ColliderHandle) -> bool,
    ) where
        Colliders: ComponentSet<ColliderFlags>
            + ComponentSet<ColliderPosition>
            + ComponentSet<ColliderShape>,
    {
        let mut leaf_callback = &mut |handle: &ColliderHandle| {
            let co_shape: Option<&ColliderShape> = colliders.get(handle.0);

            if let Some(co_shape) = co_shape {
                let (co_flags, co_pos): (&ColliderFlags, &ColliderPosition) =
                    colliders.index_bundle(handle.0);

                if co_flags.collision_groups.test(query_groups)
                    && filter.map(|f| f(*handle)).unwrap_or(true)
                    && co_shape.contains_point(co_pos, point)
                {
                    return callback(*handle);
                }
            }

            true
        };

        let mut visitor = PointIntersectionsVisitor::new(point, &mut leaf_callback);

        self.qbvh.traverse_depth_first(&mut visitor);
    }

    /// Find the projection of a point on the closest collider.
    ///
    /// The results include the ID of the feature hit by the point.
    ///
    /// # Parameters
    /// * `colliders` - The set of colliders taking part in this pipeline.
    /// * `point` - The point to project.
    /// * `solid` - If this is set to `true` then the collider shapes are considered to
    ///   be plain (if the point is located inside of a plain shape, its projection is the point
    ///   itself). If it is set to `false` the collider shapes are considered to be hollow
    ///   (if the point is located inside of an hollow shape, it is projected on the shape's
    ///   boundary).
    /// * `query_groups` - the interaction groups which will be tested against the collider's `contact_group`
    ///                   to determine if it should be taken into account by this query.
    /// * `filter` - a more fine-grained filter. A collider is taken into account by this query if
    ///             its `contact_group` is compatible with the `query_groups`, and if this `filter`
    ///             is either `None` or returns `true`.
    pub fn project_point_and_get_feature<Colliders>(
        &self,
        colliders: &Colliders,
        point: &Point<Real>,
        query_groups: InteractionGroups,
        filter: Option<&dyn Fn(ColliderHandle) -> bool>,
    ) -> Option<(ColliderHandle, PointProjection, FeatureId)>
    where
        Colliders: ComponentSet<ColliderFlags>
            + ComponentSet<ColliderPosition>
            + ComponentSet<ColliderShape>,
    {
        let pipeline_shape = self.as_composite_shape(colliders, query_groups, filter);
        let mut visitor =
            PointCompositeShapeProjWithFeatureBestFirstVisitor::new(&pipeline_shape, point, false);
        self.qbvh
            .traverse_best_first(&mut visitor)
            .map(|h| (h.1 .1 .0, h.1 .0, h.1 .1 .1))
    }

    /// Finds all handles of all the colliders with an AABB intersecting the given AABB.
    pub fn colliders_with_aabb_intersecting_aabb(
        &self,
        aabb: &AABB,
        mut callback: impl FnMut(&ColliderHandle) -> bool,
    ) {
        let mut visitor = BoundingVolumeIntersectionsVisitor::new(aabb, &mut callback);
        self.qbvh.traverse_depth_first(&mut visitor);
    }

    /// Casts a shape at a constant linear velocity and retrieve the first collider it hits.
    ///
    /// This is similar to ray-casting except that we are casting a whole shape instead of just a
    /// point (the ray origin). In the resulting `TOI`, witness and normal 1 refer to the world
    /// collider, and are in world space.
    ///
    /// # Parameters
    /// * `colliders` - The set of colliders taking part in this pipeline.
    /// * `shape_pos` - The initial position of the shape to cast.
    /// * `shape_vel` - The constant velocity of the shape to cast (i.e. the cast direction).
    /// * `shape` - The shape to cast.
    /// * `max_toi` - The maximum time-of-impact that can be reported by this cast. This effectively
    ///   limits the distance traveled by the shape to `shapeVel.norm() * maxToi`.
    /// * `query_groups` - the interaction groups which will be tested against the collider's `contact_group`
    ///                   to determine if it should be taken into account by this query.
    /// * `filter` - a more fine-grained filter. A collider is taken into account by this query if
    ///             its `contact_group` is compatible with the `query_groups`, and if this `filter`
    ///             is either `None` or returns `true`.
    pub fn cast_shape<'a, Colliders>(
        &self,
        colliders: &'a Colliders,
        shape_pos: &Isometry<Real>,
        shape_vel: &Vector<Real>,
        shape: &dyn Shape,
        max_toi: Real,
        query_groups: InteractionGroups,
        filter: Option<&dyn Fn(ColliderHandle) -> bool>,
    ) -> Option<(ColliderHandle, TOI)>
    where
        Colliders: ComponentSet<ColliderFlags>
            + ComponentSet<ColliderPosition>
            + ComponentSet<ColliderShape>,
    {
        let pipeline_shape = self.as_composite_shape(colliders, query_groups, filter);
        let mut visitor = TOICompositeShapeShapeBestFirstVisitor::new(
            &*self.query_dispatcher,
            shape_pos,
            shape_vel,
            &pipeline_shape,
            shape,
            max_toi,
        );
        self.qbvh.traverse_best_first(&mut visitor).map(|h| h.1)
    }

    /// Casts a shape with an arbitrary continuous motion and retrieve the first collider it hits.
    ///
    /// In the resulting `TOI`, witness and normal 1 refer to the world collider, and are in world
    /// space.
    ///
    /// # Parameters
    /// * `colliders` - The set of colliders taking part in this pipeline.
    /// * `shape_motion` - The motion of the shape.
    /// * `shape` - The shape to cast.
    /// * `start_time` - The starting time of the interval where the motion takes place.
    /// * `end_time` - The end time of the interval where the motion takes place.
    /// * `stop_at_penetration` - If the casted shape starts in a penetration state with any
    ///    collider, two results are possible. If `stop_at_penetration` is `true` then, the
    ///    result will have a `toi` equal to `start_time`. If `stop_at_penetration` is `false`
    ///    then the nonlinear shape-casting will see if further motion wrt. the penetration normal
    ///    would result in tunnelling. If it does not (i.e. we have a separating velocity along
    ///    that normal) then the nonlinear shape-casting will attempt to find another impact,
    ///    at a time `> start_time` that could result in tunnelling.
    /// * `query_groups` - the interaction groups which will be tested against the collider's `contact_group`
    ///                   to determine if it should be taken into account by this query.
    /// * `filter` - a more fine-grained filter. A collider is taken into account by this query if
    ///             its `contact_group` is compatible with the `query_groups`, and if this `filter`
    ///             is either `None` or returns `true`.
    pub fn nonlinear_cast_shape<Colliders>(
        &self,
        colliders: &Colliders,
        shape_motion: &NonlinearRigidMotion,
        shape: &dyn Shape,
        start_time: Real,
        end_time: Real,
        stop_at_penetration: bool,
        query_groups: InteractionGroups,
        filter: Option<&dyn Fn(ColliderHandle) -> bool>,
    ) -> Option<(ColliderHandle, TOI)>
    where
        Colliders: ComponentSet<ColliderFlags>
            + ComponentSet<ColliderPosition>
            + ComponentSet<ColliderShape>,
    {
        let pipeline_shape = self.as_composite_shape(colliders, query_groups, filter);
        let pipeline_motion = NonlinearRigidMotion::identity();
        let mut visitor = NonlinearTOICompositeShapeShapeBestFirstVisitor::new(
            &*self.query_dispatcher,
            &pipeline_motion,
            &pipeline_shape,
            shape_motion,
            shape,
            start_time,
            end_time,
            stop_at_penetration,
        );
        self.qbvh.traverse_best_first(&mut visitor).map(|h| h.1)
    }

    /// Retrieve all the colliders intersecting the given shape.
    ///
    /// # Parameters
    /// * `colliders` - The set of colliders taking part in this pipeline.
    /// * `shapePos` - The position of the shape to test.
    /// * `shapeRot` - The orientation of the shape to test.
    /// * `shape` - The shape to test.
    /// * `query_groups` - the interaction groups which will be tested against the collider's `contact_group`
    ///                   to determine if it should be taken into account by this query.
    /// * `filter` - a more fine-grained filter. A collider is taken into account by this query if
    ///             its `contact_group` is compatible with the `query_groups`, and if this `filter`
    ///             is either `None` or returns `true`.
    /// * `callback` - A function called with the handles of each collider intersecting the `shape`.
    pub fn intersections_with_shape<'a, Colliders>(
        &self,
        colliders: &'a Colliders,
        shape_pos: &Isometry<Real>,
        shape: &dyn Shape,
        query_groups: InteractionGroups,
        filter: Option<&dyn Fn(ColliderHandle) -> bool>,
        mut callback: impl FnMut(ColliderHandle) -> bool,
    ) where
        Colliders: ComponentSet<ColliderFlags>
            + ComponentSet<ColliderPosition>
            + ComponentSet<ColliderShape>,
    {
        let dispatcher = &*self.query_dispatcher;
        let inv_shape_pos = shape_pos.inverse();

        let mut leaf_callback = &mut |handle: &ColliderHandle| {
            let co_shape: Option<&ColliderShape> = colliders.get(handle.0);

            if let Some(co_shape) = co_shape {
                let (co_flags, co_pos): (&ColliderFlags, &ColliderPosition) =
                    colliders.index_bundle(handle.0);

                if co_flags.collision_groups.test(query_groups)
                    && filter.map(|f| f(*handle)).unwrap_or(true)
                {
                    let pos12 = inv_shape_pos * co_pos.as_ref();

                    if dispatcher.intersection_test(&pos12, shape, &**co_shape) == Ok(true) {
                        return callback(*handle);
                    }
                }
            }

            true
        };

        let shape_aabb = shape.compute_aabb(shape_pos);
        let mut visitor = BoundingVolumeIntersectionsVisitor::new(&shape_aabb, &mut leaf_callback);

        self.qbvh.traverse_depth_first(&mut visitor);
    }
}
