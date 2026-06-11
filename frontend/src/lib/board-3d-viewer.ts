/**
 * board-3d-viewer.ts
 *
 * Three.js-based 3D board renderer. Lazy-loaded only when the 3D tab is active.
 *
 * COORDINATE TRANSFORM (KiCad mm -> Three.js world metres):
 *
 *   KiCad uses millimetres with Y increasing downward.
 *   The KiCad GLB export places the board so that:
 *     - KiCad X maps to Three.js +X  (multiply by 0.001)
 *     - KiCad Y maps to Three.js -Z  (negate, multiply by 0.001) because GLB is Y-up
 *     - PCB surface sits at Three.js Y ≈ 0; component tops rise in +Y
 *
 *   So for a footprint at KiCad (x_mm, y_mm):
 *       threeX = x_mm * 0.001
 *       threeY = PCB_TOP_Y  (≈ 0.0016 – top of 1.6 mm board)
 *       threeZ = -y_mm * 0.001
 *
 *   Verified empirically: pic_programmer has an axial cap C1 at KiCad (110.49, 78.867) mm.
 *   In the loaded GLB that footprint appears at approx (0.1105, ~0.002, -0.0789) m — matching
 *   this formula within floating-point rounding.
 */

import * as THREE from 'three'
import { GLTFLoader } from 'three/examples/jsm/loaders/GLTFLoader.js'
import { OrbitControls } from 'three/examples/jsm/controls/OrbitControls.js'
import type { ParsedBoard } from './kicad-parser'

// KiCad PCB standard 1.6 mm → top surface in Three.js
const PCB_TOP_Y = 0.0016

export interface Highlight3D {
  reference: string
  color: number  // hex color e.g. 0x22d3ee
  intensity: number  // 0..1
}

export class Board3DViewer {
  private renderer: THREE.WebGLRenderer
  private scene: THREE.Scene
  private camera: THREE.PerspectiveCamera
  private controls: OrbitControls
  private markerGroup: THREE.Group
  private markerMeshes: Map<string, THREE.Mesh> = new Map()
  private animHandle: number | null = null
  private disposed = false

  constructor(canvas: HTMLCanvasElement) {
    // Renderer
    this.renderer = new THREE.WebGLRenderer({ canvas, antialias: true, alpha: false })
    this.renderer.setPixelRatio(Math.min(window.devicePixelRatio, 2))
    this.renderer.setSize(canvas.clientWidth, canvas.clientHeight, false)
    this.renderer.shadowMap.enabled = true
    this.renderer.shadowMap.type = THREE.PCFSoftShadowMap
    this.renderer.outputColorSpace = THREE.SRGBColorSpace
    this.renderer.toneMapping = THREE.ACESFilmicToneMapping
    this.renderer.toneMappingExposure = 1.2

    // Scene
    this.scene = new THREE.Scene()
    this.scene.background = new THREE.Color(0x020617)
    this.scene.fog = new THREE.FogExp2(0x020617, 4)

    // Camera
    this.camera = new THREE.PerspectiveCamera(45, canvas.clientWidth / canvas.clientHeight, 0.001, 10)
    this.camera.position.set(0.15, 0.12, 0.18)
    this.camera.lookAt(0.12, 0, -0.08)

    // Studio lighting: hemisphere + key + rim + ambient
    const hemi = new THREE.HemisphereLight(0x88aadd, 0x443322, 1.6)
    this.scene.add(hemi)

    const keyLight = new THREE.DirectionalLight(0xfff5e0, 3.2)
    keyLight.position.set(0.3, 0.5, 0.2)
    keyLight.castShadow = true
    keyLight.shadow.mapSize.set(2048, 2048)
    keyLight.shadow.camera.near = 0.001
    keyLight.shadow.camera.far = 2
    keyLight.shadow.camera.left = -0.3
    keyLight.shadow.camera.right = 0.3
    keyLight.shadow.camera.top = 0.3
    keyLight.shadow.camera.bottom = -0.3
    keyLight.shadow.bias = -0.0005
    this.scene.add(keyLight)

    const rimLight = new THREE.DirectionalLight(0x8090ff, 0.5)
    rimLight.position.set(-0.3, 0.2, -0.3)
    this.scene.add(rimLight)

    const fill = new THREE.DirectionalLight(0xffffff, 0.3)
    fill.position.set(0, 0.4, 0.4)
    this.scene.add(fill)

    // Ground shadow plane (receives shadows only)
    const groundGeo = new THREE.PlaneGeometry(2, 2)
    const groundMat = new THREE.ShadowMaterial({ opacity: 0.25 })
    const ground = new THREE.Mesh(groundGeo, groundMat)
    ground.rotation.x = -Math.PI / 2
    ground.position.y = -0.001
    ground.receiveShadow = true
    this.scene.add(ground)

    // Marker group for component overlays
    this.markerGroup = new THREE.Group()
    this.scene.add(this.markerGroup)

    // Orbit controls with smooth damping
    this.controls = new OrbitControls(this.camera, canvas)
    this.controls.enableDamping = true
    this.controls.dampingFactor = 0.06
    this.controls.minDistance = 0.02
    this.controls.maxDistance = 1.0
    this.controls.maxPolarAngle = Math.PI / 2 + 0.15
    this.controls.target.set(0.12, 0, -0.08)
    this.controls.update()

    this.startLoop()
  }

  /** Load a GLB file. Returns a promise resolving when loaded. */
  async loadGLB(url: string): Promise<void> {
    const loader = new GLTFLoader()
    const gltf = await loader.loadAsync(url)
    const model = gltf.scene

    // Center camera on loaded model
    const box = new THREE.Box3().setFromObject(model)
    const center = new THREE.Vector3()
    box.getCenter(center)
    const size = new THREE.Vector3()
    box.getSize(size)
    const maxDim = Math.max(size.x, size.y, size.z)

    this.controls.target.copy(center)
    this.camera.position.set(
      center.x + maxDim * 0.6,
      center.y + maxDim * 0.9,
      center.z + maxDim * 1.1,
    )
    this.camera.near = maxDim * 0.001
    this.camera.far = maxDim * 20
    this.camera.updateProjectionMatrix()
    this.controls.update()

    // Enable shadows on all meshes
    model.traverse(obj => {
      if ((obj as THREE.Mesh).isMesh) {
        obj.castShadow = true
        obj.receiveShadow = true
      }
    })

    // Remove any previous board model (keep lights, ground, markers)
    const toRemove: THREE.Object3D[] = []
    this.scene.traverse(obj => {
      if (obj !== this.scene && obj !== this.markerGroup && !(obj instanceof THREE.Light)
        && !(obj instanceof THREE.Mesh && (obj.material as THREE.ShadowMaterial)?.opacity !== undefined)) {
        if (!obj.parent || obj.parent === this.scene) toRemove.push(obj)
      }
    })
    // Actually: just add and track separately
    // Simpler: remove by name
    const existing = this.scene.getObjectByName('__board_model__')
    if (existing) this.scene.remove(existing)

    model.name = '__board_model__'
    this.scene.add(model)
  }

  /**
   * Update or create glowing marker sprites for active components.
   * positions: ref -> {x, y} in KiCad mm (from ParsedBoard.footprints)
   */
  updateMarkers(
    board: ParsedBoard,
    componentStates: Record<string, Record<string, number>>,
    componentKinds: Record<string, string>,
    faults?: { component: string; fault_kind: string; value: number; limit: number; t: number }[],
  ) {
    const activeRefs = new Set<string>()
    const faultRefs = new Set<string>(faults?.map(f => f.component) ?? [])

    for (const fp of board.footprints) {
      const states = componentStates[fp.ref]
      const kind = componentKinds?.[fp.ref]
      if (!states && !faultRefs.has(fp.ref)) continue

      const running = states?.['running'] ?? 0
      const dissipation = states?.['dissipation_mw'] ?? 0

      let color = 0x22d3ee  // cyan default
      let intensity = 0

      if (faultRefs.has(fp.ref)) {
        color = 0xff2222
        intensity = 1.0
      } else if (kind === 'mcu' && running > 0) {
        color = 0x22d3ee
        intensity = running
      } else if (dissipation > 0) {
        const t = Math.min(1, dissipation / 500)
        // heat: blue -> yellow -> red
        const r = Math.min(255, t * 2 * 255)
        const g = Math.min(255, (1 - Math.abs(t - 0.5) * 2) * 200)
        const b = Math.max(0, (1 - t * 2) * 255)
        color = (Math.round(r) << 16) | (Math.round(g) << 8) | Math.round(b)
        intensity = t * 0.7 + 0.1
      } else {
        continue
      }

      activeRefs.add(fp.ref)

      // Marker position: KiCad mm -> Three.js metres
      const x3 = fp.at.x * 0.001
      const z3 = -fp.at.y * 0.001
      const y3 = PCB_TOP_Y + 0.002

      let mesh = this.markerMeshes.get(fp.ref)
      if (!mesh) {
        const geo = new THREE.SphereGeometry(0.0015, 12, 8)
        const mat = new THREE.MeshBasicMaterial({ color, transparent: true, opacity: 0.9 })
        mesh = new THREE.Mesh(geo, mat)
        this.markerGroup.add(mesh)
        this.markerMeshes.set(fp.ref, mesh)
      }

      mesh.position.set(x3, y3, z3)
      const mat = mesh.material as THREE.MeshBasicMaterial
      mat.color.setHex(color)
      mat.opacity = 0.6 + intensity * 0.4
      mesh.visible = true
    }

    // Hide stale markers
    for (const [ref, mesh] of this.markerMeshes) {
      if (!activeRefs.has(ref)) mesh.visible = false
    }
  }

  /** Programmatic highlight API for external callers. */
  set3dHighlight(reference: string, color: number, intensity: number) {
    let mesh = this.markerMeshes.get(reference)
    if (!mesh) {
      const geo = new THREE.SphereGeometry(0.0015, 12, 8)
      const mat = new THREE.MeshBasicMaterial({ color, transparent: true, opacity: 0.9 })
      mesh = new THREE.Mesh(geo, mat)
      this.markerGroup.add(mesh)
      this.markerMeshes.set(reference, mesh)
    }
    const mat = mesh.material as THREE.MeshBasicMaterial
    mat.color.setHex(color)
    mat.opacity = 0.5 + intensity * 0.5
    mesh.visible = true
  }

  setSize(w: number, h: number) {
    this.renderer.setSize(w, h, false)
    this.camera.aspect = w / h
    this.camera.updateProjectionMatrix()
  }

  private startLoop() {
    const tick = () => {
      if (this.disposed) return
      this.animHandle = requestAnimationFrame(tick)
      this.controls.update()
      this.renderer.render(this.scene, this.camera)
    }
    this.animHandle = requestAnimationFrame(tick)
  }

  dispose() {
    this.disposed = true
    if (this.animHandle !== null) cancelAnimationFrame(this.animHandle)
    this.controls.dispose()
    this.renderer.dispose()
  }
}
