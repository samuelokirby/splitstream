// src/audio_input_buffers.rs
//! AudioInputBuffers manages ring buffers for microphone and system audio input.
//! They are used inside of the callback to store incoming audio samples.
use ringbuf::{HeapCons, HeapProd, HeapRb, traits::Split};

// We define a struct that holds the producers and consumers
// for both microphone and system audio ring buffers.
pub struct AudioInputBuffers {
    pub mic_producer: HeapProd<f32>,
    pub sys_producer: HeapProd<f32>,
    pub mic_consumer: HeapCons<f32>,
    pub sys_consumer: HeapCons<f32>,
}

impl AudioInputBuffers {
    /// Constructor that creates ring buffers of the specified size
    /// and splits them into producers and consumers.
    /// Arguments:
    /// * `buffer_size`: usize, the size of each ring buffer
    /// Returns:
    /// * `AudioInputBuffers`: the struct containing producers and consumers
    pub fn new(buffer_size: usize) -> Self {
        // Create two ring buffers, one for mic and one for system audio
        let mic_rb = HeapRb::<f32>::new(buffer_size);
        let sys_rb = HeapRb::<f32>::new(buffer_size);

        // Split each ring buffer into a producer and consumer
        let (mic_producer, mic_consumer) = mic_rb.split();
        let (sys_producer, sys_consumer) = sys_rb.split();

        // Return the struct with all four producers and consumers
        Self {
            mic_producer,
            sys_producer,
            mic_consumer,
            sys_consumer,
        }
    }

    /// This takes ownership of self, yields the producers and consumers
    /// as tuples, and leaves self unusable.
    /// This is useful for moving the producers into the audio callback
    /// and the consumers into the main processing loop.
    /// Returns:
    /// * `((HeapProd<f32>, HeapProd<f32>), (HeapCons<f32>, HeapCons<f32>))`:
    ///   A tuple containing two tuples:
    ///   - First tuple: (mic_producer, sys_producer)
    ///   - Second tuple: (mic_consumer, sys_consumer)
    pub fn into_buffer_splits(
        self,
    ) -> (
        (HeapProd<f32>, HeapProd<f32>),
        (HeapCons<f32>, HeapCons<f32>),
    ) {
        (
            (self.mic_producer, self.sys_producer),
            (self.mic_consumer, self.sys_consumer),
        )
    }
}
