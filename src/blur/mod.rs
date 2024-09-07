mod gaussian;

use gaussian::RecursiveGaussian;

/// Structure handling image blur.
///
/// This struct contains the necessary buffers and the kernel used for blurring
/// (currently a recursive approximation of the Gaussian filter).
///
/// Note that the width and height of the image passed to [blur][Self::blur] needs to exactly
/// match the width and height of this instance. If you reduce the image size (e.g. via
/// downscaling), [`shrink_to`][Self::shrink_to] can be used to resize the internal buffers.
pub struct Blur {
    kernel: RecursiveGaussian,
    temp: Vec<f32>,
    width: usize,
    height: usize,
}

impl Blur {
    /// Create a new [Blur] for images of the given width and height.
    /// This pre-allocates the necessary buffers.
    #[must_use]
    pub fn new(width: usize, height: usize) -> Self {
        Blur {
            kernel: RecursiveGaussian,
            temp: vec![0.0f32; width * height],
            width,
            height,
        }
    }

    /// Truncates the internal buffers to fit images of the given width and height.
    ///
    /// This will [truncate][Vec::truncate] the internal buffers
    /// without affecting the allocated memory.
    pub fn shrink_to(&mut self, width: usize, height: usize) {
        self.temp.truncate(width * height);
        self.width = width;
        self.height = height;
    }

    /// Blur the given image.
    #[cfg(feature = "fast-blur")]
    pub fn blur(&mut self, img: &[Vec<f32>; 3]) -> [Vec<f32>; 3] {
        use libblur::*;
        use bytemuck::*;
    
        let mut flattened: Vec<f32> = Vec::new();
        
        for i in 0..img[0].len() {
            flattened.push(img[0][i]);
            flattened.push(img[1][i]);
            flattened.push(img[2][i]);
        }

        //let mut image_copy = img.clone();
        //let mut image_copy_a = img[0].clone();
        //let mut image_copy_b = img[1].clone();
        //let mut image_copy_c = img[2].clone();
        //let byte_slice_static [f32] = bytemuck::cast_slice_mut(&mut flattened);
        let flatten_clone = flattened.clone();
        let byte_slice_static = bytemuck::cast_slice(&flatten_clone);
        let byte_slice: &mut [f32] = bytemuck::cast_slice_mut(&mut flattened);
        //let byte_slice_static = byte_slice.clone();
        //let byte_slice_a: &mut [f32] = bytemuck::cast_slice_mut(&mut image_copy_a);
        //let byte_slice_a: &mut [f32] = bytemuck::cast_slice_mut(&mut image_copy_a);
        //let byte_slice_b: &mut [f32] = bytemuck::cast_slice_mut(&mut image_copy_b);
        //let byte_slice_c: &mut [f32] = bytemuck::cast_slice_mut(&mut image_copy_c);
        
        libblur::box_blur_f32(
            byte_slice_static,
            byte_slice,
            //self.width as u32 * 3,
            self.width as u32,
            self.height as u32, 
            2, 
            FastBlurChannels::Channels3,
            libblur::ThreadingPolicy::Adaptive,
            //libblur::EdgeMode::Clamp
        );

        //libblur::fast_gaussian_f32(
        //    byte_slice,
        //    //self.width as u32 * 3,
        //    self.width as u32,
        //    self.height as u32, 
        //    2, 
        //    FastBlurChannels::Channels3,
        //    libblur::ThreadingPolicy::Adaptive,
        //    libblur::EdgeMode::Clamp
        //);

        //libblur::fast_gaussian_plane_f32(
        //    byte_slice_a,
        //    //self.width as u32 * 3,
        //    self.width as u32,
        //    self.height as u32, 
        //    2, 
        //    //FastBlurChannels::Channels3,
        //    libblur::ThreadingPolicy::Adaptive,
        //    libblur::EdgeMode::Clamp
        //);
        //libblur::fast_gaussian_plane_f32(
        //    byte_slice_b,
        //    //self.width as u32 * 3,
        //    self.width as u32,
        //    self.height as u32, 
        //    2, 
        //    //FastBlurChannels::Channels3,
        //    libblur::ThreadingPolicy::Adaptive,
        //    libblur::EdgeMode::Clamp
        //);
        //libblur::fast_gaussian_plane_f32(
        //    byte_slice_c,
        //    //self.width as u32 * 3,
        //    self.width as u32,
        //    self.height as u32, 
        //    2, 
        //    //FastBlurChannels::Channels3,
        //    libblur::ThreadingPolicy::Adaptive,
        //    libblur::EdgeMode::Clamp
        //);

        //libblur::tent_blur_f32(
        //    byte_slice, 
        //    //self.width as u32 * 3, 
        //    self.width as u32, 
        //    self.height as u32, 
        //    1, 
        //    FastBlurChannels::Channels3,
        //    libblur::ThreadingPolicy::Adaptive,
        //);

        let flat_blur = cast_slice(byte_slice).to_vec();
        //let flat_blur_a = cast_slice(byte_slice_a).to_vec();
        //let flat_blur_b = cast_slice(byte_slice_b).to_vec();
        //let flat_blur_c = cast_slice(byte_slice_c).to_vec();

        let mut img: [Vec<f32>; 3] = [Vec::new(), Vec::new(), Vec::new()];
        for i in 0..flat_blur.len() / 3 {
            img[0].push(flat_blur[3 * i]);
            img[1].push(flat_blur[3 * i + 1]);
            img[2].push(flat_blur[3 * i + 2]);
        }
        return img;
        //[
        //    flat_blur_a,
        //    flat_blur_b,
        //    flat_blur_c
        //]

    }

    #[cfg(not(feature = "fast-blur"))]
    pub fn blur(&mut self, img: &[Vec<f32>; 3]) -> [Vec<f32>; 3] {
        [
            self.blur_plane(&img[0]),
            self.blur_plane(&img[1]),
            self.blur_plane(&img[2]),
        ]
    }

    fn blur_plane(&mut self, plane: &[f32]) -> Vec<f32> {
        let mut out = vec![0f32; self.width * self.height];
        self.kernel
            .horizontal_pass(plane, &mut self.temp, self.width);
        self.kernel
            .vertical_pass_chunked::<128, 32>(&self.temp, &mut out, self.width, self.height);
        out
    }
}
