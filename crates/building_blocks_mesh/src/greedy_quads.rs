use super::{
    quad::{OrientedCubeFace, UnorientedQuad},
    IsOpaque,
};

use building_blocks_core::{axis::Axis3Permutation, prelude::*};
use building_blocks_storage::{access::GetUncheckedRelease, prelude::*};

/// Contains the output from the `greedy_quads` algorithm. The quads can be used to generate a mesh. See the methods on
/// `OrientedCubeFace` and `UnorientedQuad` for details.
///
/// This buffer can be reused between multiple calls of `greedy_quads` in order to avoid reallocations.
pub struct GreedyQuadsBuffer {
    /// One group of quads per cube face.
    pub quad_groups: [QuadGroup; 6],

    // A single array is used for the visited mask because it allows us to index by the same strides as the voxels array. It
    // also only requires a single allocation.
    visited: Array3<bool>,
}

/// A set of quads that share an orientation.
pub struct QuadGroup {
    /// The quads themselves. We rely on the `face` metadata to interpret them.
    ///
    /// When using these values for materials and lighting, you can access them using either the quad's minimum voxel
    /// coordinates or the vertex coordinates given by `OrientedCubeFace::quad_corners`.
    pub quads: Vec<UnorientedQuad>,
    /// One of 6 cube faces. All quads in this struct are comprised of only this face.
    pub face: OrientedCubeFace,
}

impl QuadGroup {
    pub fn new(face: OrientedCubeFace) -> Self {
        Self {
            quads: Vec::new(),
            face,
        }
    }
}

impl GreedyQuadsBuffer {
    pub fn new(extent: Extent3i) -> Self {
        let quad_groups = [
            QuadGroup::new(OrientedCubeFace::new(-1, Axis3Permutation::XZY)),
            QuadGroup::new(OrientedCubeFace::new(-1, Axis3Permutation::YXZ)),
            QuadGroup::new(OrientedCubeFace::new(-1, Axis3Permutation::ZXY)),
            QuadGroup::new(OrientedCubeFace::new(1, Axis3Permutation::XZY)),
            QuadGroup::new(OrientedCubeFace::new(1, Axis3Permutation::YXZ)),
            QuadGroup::new(OrientedCubeFace::new(1, Axis3Permutation::ZXY)),
        ];

        Self {
            quad_groups,
            visited: Array3::fill(extent, false),
        }
    }

    pub fn reset(&mut self, extent: Extent3i) {
        for group in self.quad_groups.iter_mut() {
            group.quads.clear();
        }

        if extent.shape != self.visited.extent().shape {
            self.visited = Array3::fill(extent, false);
        }
        self.visited.set_minimum(extent.minimum);
    }

    /// Returns the total count of quads across all groups.
    pub fn num_quads(&self) -> usize {
        let mut sum = 0;
        for group in self.quad_groups.iter() {
            sum += group.quads.len();
        }

        sum
    }
}

/// Pads the given chunk extent with exactly the amount of space required for running the
/// `greedy_quads` algorithm.
pub fn padded_greedy_quads_chunk_extent(chunk_extent: &Extent3i) -> Extent3i {
    chunk_extent.padded(1)
}

/// The "Greedy Meshing" algorithm described by Mikola Lysenko in the [0fps
/// article](https://0fps.net/2012/06/30/meshing-in-a-minecraft-game/).
///
/// All visible faces of voxels on the interior of `extent` will be part of some `Quad` returned via the `output` buffer. A
/// 3x3x3 kernel will be applied to each point on the interior, hence the extra padding required on the boundary. `voxels` only
/// needs to contain the set of points in `extent`.
///
/// All quads created will have the same "merge value" as defined by the `MergeVoxel` trait. The quads can be post-processed
/// into meshes as the user sees fit.
pub fn greedy_quads<A, T>(voxels: &A, extent: &Extent3i, output: &mut GreedyQuadsBuffer)
where
    A: Array<[i32; 3]>
        + GetUncheckedRelease<Stride, T>
        + ForEach<[i32; 3], (Point3i, Stride), Data = T>,
    T: IsEmpty + IsOpaque + MergeVoxel,
{
    greedy_quads_with_merge_strategy::<_, _, VoxelMerger<T>>(voxels, extent, output)
}

/// Run the greedy meshing algorithm with a custom quad merging strategy using the `MergeStrategy` trait.
pub fn greedy_quads_with_merge_strategy<A, T, Merger>(
    voxels: &A,
    extent: &Extent3i,
    output: &mut GreedyQuadsBuffer,
) where
    A: Array<[i32; 3]>
        + GetUncheckedRelease<Stride, T>
        + ForEach<[i32; 3], (Point3i, Stride), Data = T>,
    T: IsEmpty + IsOpaque,
    Merger: MergeStrategy<Voxel = T>,
{
    output.reset(*extent);
    let GreedyQuadsBuffer {
        visited,
        quad_groups,
    } = output;

    let interior = extent.padded(-1); // Avoid accessing out of bounds with a 3x3x3 kernel.

    for group in quad_groups.iter_mut() {
        greedy_quads_for_group::<_, _, Merger>(voxels, interior, visited, group);
    }
}

fn greedy_quads_for_group<A, T, Merger>(
    voxels: &A,
    interior: Extent3i,
    visited: &mut Array3<bool>,
    quad_group: &mut QuadGroup,
) where
    A: Array<[i32; 3]>
        + GetUncheckedRelease<Stride, T>
        + ForEach<[i32; 3], (Point3i, Stride), Data = T>,
    T: IsEmpty + IsOpaque,
    Merger: MergeStrategy<Voxel = T>,
{
    visited.reset_values(false);

    let QuadGroup {
        quads,
        face:
            OrientedCubeFace {
                n_sign,
                permutation,
                n,
                u,
                v,
                ..
            },
    } = quad_group;

    let [n_axis, u_axis, v_axis] = permutation.axes();
    let i_n = n_axis.index();
    let i_u = u_axis.index();
    let i_v = v_axis.index();

    let num_slices = interior.shape.at(i_n);
    let slice_shape = *n + *u * interior.shape.at(i_u) + *v * interior.shape.at(i_v);
    let mut slice_extent = Extent3i::from_min_and_shape(interior.minimum, slice_shape);

    let n_stride = voxels.stride_from_local_point(&Local(*n));
    let u_stride = voxels.stride_from_local_point(&Local(*u));
    let v_stride = voxels.stride_from_local_point(&Local(*v));
    let face_strides = FaceStrides {
        n_stride,
        u_stride,
        v_stride,
        // The offset to the voxel sharing this cube face.
        visibility_offset: if *n_sign > 0 {
            n_stride
        } else {
            Stride(0) - n_stride
        },
    };

    for _ in 0..num_slices {
        let slice_ub = slice_extent.least_upper_bound();
        let u_ub = slice_ub.at(i_u);
        let v_ub = slice_ub.at(i_v);

        voxels.for_each(
            &slice_extent,
            |(quad_min, quad_min_stride): (Point3i, Stride), quad_min_voxel| {
                if !face_needs_mesh(
                    &quad_min_voxel,
                    quad_min_stride,
                    face_strides.visibility_offset,
                    voxels,
                    visited,
                ) {
                    return;
                }
                // We have at least one face that needs a mesh. We'll try to expand that face into the biggest quad we can find.

                // These are the boundaries on quad width and height so it is contained in the slice.
                let max_width = u_ub - quad_min.at(i_u);
                let max_height = v_ub - quad_min.at(i_v);

                let (quad_width, quad_height) = Merger::find_quad(
                    quad_min_stride,
                    &quad_min_voxel,
                    max_width,
                    max_height,
                    &face_strides,
                    voxels,
                    &visited,
                );
                debug_assert!(quad_width >= 1);
                debug_assert!(quad_width <= max_width);
                debug_assert!(quad_height >= 1);
                debug_assert!(quad_height <= max_height);

                // Mark the quad as visited.
                let quad_extent =
                    Extent3i::from_min_and_shape(quad_min, *n + *u * quad_width + *v * quad_height);
                visited.fill_extent(&quad_extent, true);

                quads.push(UnorientedQuad {
                    minimum: quad_min,
                    width: quad_width,
                    height: quad_height,
                });
            },
        );

        // Move to the next slice.
        slice_extent += *n;
    }
}

/// Returns true iff the given `voxel` face needs to be meshed. This means that we haven't already meshed it, it is non-empty,
/// and it's visible (not completely occluded by an adjacent voxel).
#[inline]
fn face_needs_mesh<A, T>(
    voxel: &T,
    voxel_stride: Stride,
    visibility_offset: Stride,
    voxels: &A,
    visited: &Array3<bool>,
) -> bool
where
    A: GetUncheckedRelease<Stride, T>,
    T: IsEmpty + IsOpaque,
{
    if voxel.is_empty() || visited.get_unchecked_release(voxel_stride) {
        return false;
    }

    let adjacent_voxel = voxels.get_unchecked_release(voxel_stride + visibility_offset);

    !adjacent_voxel.is_opaque() || adjacent_voxel.is_empty()
}

// ███╗   ███╗███████╗██████╗  ██████╗ ███████╗██████╗ ███████╗
// ████╗ ████║██╔════╝██╔══██╗██╔════╝ ██╔════╝██╔══██╗██╔════╝
// ██╔████╔██║█████╗  ██████╔╝██║  ███╗█████╗  ██████╔╝███████╗
// ██║╚██╔╝██║██╔══╝  ██╔══██╗██║   ██║██╔══╝  ██╔══██╗╚════██║
// ██║ ╚═╝ ██║███████╗██║  ██║╚██████╔╝███████╗██║  ██║███████║
// ╚═╝     ╚═╝╚══════╝╚═╝  ╚═╝ ╚═════╝ ╚══════╝╚═╝  ╚═╝╚══════╝

/// A strategy for merging cube faces into quads.
pub trait MergeStrategy {
    type Voxel;

    /// Return the width and height of the quad that should be constructed.
    ///
    /// `min_stride`: The `Stride` of `min`.
    ///
    /// `min_value`: The voxel value for `min`.
    ///
    /// `max_width`: The maximum possible width for the quad to be constructed.
    ///
    /// `max_height`: The maximum possible height for the quad to be constructed.
    ///
    /// `face_strides`: Strides to help with indexing in the necessary directions for this cube face.
    ///
    /// `voxels`: The entire array of voxel data, indexed by `Stride`.
    ///
    /// `visited`: The bitmask of which voxels have already been meshed. A quad's extent will be marked as visited (`true`)
    ///            after `find_quad` returns.
    fn find_quad<A>(
        min_stride: Stride,
        min_value: &Self::Voxel,
        max_width: i32,
        max_height: i32,
        face_strides: &FaceStrides,
        voxels: &A,
        visited: &Array3<bool>,
    ) -> (i32, i32)
    where
        A: Array<[i32; 3]> + GetUncheckedRelease<Stride, Self::Voxel>,
        Self::Voxel: IsEmpty + IsOpaque;
}

pub struct FaceStrides {
    pub n_stride: Stride,
    pub u_stride: Stride,
    pub v_stride: Stride,
    pub visibility_offset: Stride,
}

/// A per-voxel value used for merging quads.
pub trait MergeVoxel {
    type VoxelValue: Eq;

    /// The value used to determine if this voxel can join a given quad in the mesh. This value will be constant for all voxels
    /// in the same quad. Often this is some material identifier so that the same texture can be used for a full quad.
    fn voxel_merge_value(&self) -> Self::VoxelValue;
}

struct VoxelMerger<T> {
    marker: std::marker::PhantomData<T>,
}

impl<T> MergeStrategy for VoxelMerger<T>
where
    T: MergeVoxel + IsEmpty + IsOpaque,
{
    type Voxel = T;

    fn find_quad<A>(
        min_stride: Stride,
        min_value: &T,
        mut max_width: i32,
        max_height: i32,
        face_strides: &FaceStrides,
        voxels: &A,
        visited: &Array3<bool>,
    ) -> (i32, i32)
    where
        A: GetUncheckedRelease<Stride, T>,
    {
        // Greedily search for the biggest visible quad where all merge values are the same.
        let quad_value = min_value.voxel_merge_value();

        // Start by finding the widest quad in the U direction.
        let mut row_start_stride = min_stride;
        let quad_width = Self::get_row_width(
            voxels,
            visited,
            &quad_value,
            face_strides.visibility_offset,
            row_start_stride,
            face_strides.u_stride,
            max_width,
        );

        // Now see how tall we can make the quad in the V direction without changing the width.
        max_width = max_width.min(quad_width);
        row_start_stride += face_strides.v_stride;
        let mut quad_height = 1;
        while quad_height < max_height {
            let row_width = Self::get_row_width(
                voxels,
                visited,
                &quad_value,
                face_strides.visibility_offset,
                row_start_stride,
                face_strides.u_stride,
                max_width,
            );
            if row_width < quad_width {
                break;
            }
            quad_height += 1;
            row_start_stride += face_strides.v_stride;
        }

        (quad_width, quad_height)
    }
}

impl<T> VoxelMerger<T> {
    fn get_row_width<A>(
        voxels: &A,
        visited: &Array3<bool>,
        quad_merge_voxel_value: &T::VoxelValue,
        visibility_offset: Stride,
        start_stride: Stride,
        delta_stride: Stride,
        max_width: i32,
    ) -> i32
    where
        A: GetUncheckedRelease<Stride, T>,
        T: IsEmpty + IsOpaque + MergeVoxel,
    {
        let mut quad_width = 0;
        let mut row_stride = start_stride;
        while quad_width < max_width {
            if visited.get_unchecked_release(row_stride) {
                // Already have a quad for this voxel face.
                break;
            }

            let voxel = voxels.get_unchecked_release(row_stride);
            if voxel.is_empty() || !voxel.voxel_merge_value().eq(quad_merge_voxel_value) {
                // Voxel needs to be non-empty and match the quad merge value.
                break;
            }

            let adjacent_voxel = voxels.get_unchecked_release(row_stride + visibility_offset);
            if adjacent_voxel.is_opaque() && !adjacent_voxel.is_empty() {
                // The adjacent voxel is occluding this face.
                break;
            }

            quad_width += 1;
            row_stride += delta_stride;
        }

        quad_width
    }
}

// TODO: implement a MergeStrategy for voxels with an ambient occlusion value at each vertex
