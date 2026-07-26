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
 *   In the loaded GLB that footprint appears at approx (0.1105, ~0.002, -0.0789) m, matching
 *   this formula within floating-point rounding.
 */

import * as THREE from 'three'
import { GLTFLoader } from 'three/examples/jsm/loaders/GLTFLoader.js'
import { OrbitControls } from 'three/examples/jsm/controls/OrbitControls.js'
import { RoomEnvironment } from 'three/examples/jsm/environments/RoomEnvironment.js'
import { EffectComposer } from 'three/examples/jsm/postprocessing/EffectComposer.js'
import { RenderPass } from 'three/examples/jsm/postprocessing/RenderPass.js'
import { SSAOPass } from 'three/examples/jsm/postprocessing/SSAOPass.js'
import { OutputPass } from 'three/examples/jsm/postprocessing/OutputPass.js'
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
  private composer: EffectComposer

  constructor(canvas: HTMLCanvasElement) {
    // Renderer, alpha:true so the CSS gradient underneath is visible through the canvas
    this.renderer = new THREE.WebGLRenderer({ canvas, antialias: true, alpha: true })
    this.renderer.setPixelRatio(Math.min(window.devicePixelRatio, 2))
    const rW = canvas.width > 0 ? canvas.width : (canvas.clientWidth > 0 ? canvas.clientWidth : 1280)
    const rH = canvas.height > 0 ? canvas.height : (canvas.clientHeight > 0 ? canvas.clientHeight : 800)
    this.renderer.setSize(rW, rH, false)
    this.renderer.shadowMap.enabled = true
    this.renderer.shadowMap.type = THREE.PCFSoftShadowMap
    this.renderer.outputColorSpace = THREE.SRGBColorSpace
    this.renderer.toneMapping = THREE.ACESFilmicToneMapping
    this.renderer.toneMappingExposure = 1.6
    this.renderer.setClearColor(0x000000, 0) // fully transparent clear

    // Scene
    this.scene = new THREE.Scene()
    // No scene.background, let the CSS radial gradient show through the alpha channel
    // Subtle depth fog, colour matched to background so it doesn't tint the board
    this.scene.fog = new THREE.FogExp2(0x020617, 1.5)

    // Environment map (RoomEnvironment, cheap PMREM IBL, transforms PBR soldermask from flat to shiny)
    const pmremGen = new THREE.PMREMGenerator(this.renderer)
    pmremGen.compileEquirectangularShader()
    const roomEnv = new RoomEnvironment()
    const envTexture = pmremGen.fromScene(roomEnv, 0.04).texture
    this.scene.environment = envTexture
    roomEnv.dispose()
    pmremGen.dispose()

    // Camera, use canvas pixel dimensions (set by attribute) with safe fallback
    const initW = canvas.width > 0 ? canvas.width : (canvas.clientWidth > 0 ? canvas.clientWidth : 1280)
    const initH = canvas.height > 0 ? canvas.height : (canvas.clientHeight > 0 ? canvas.clientHeight : 800)
    this.camera = new THREE.PerspectiveCamera(45, initW / initH, 0.001, 10)
    this.camera.position.set(0.15, 0.12, 0.18)
    this.camera.lookAt(0.12, 0, -0.08)
    this.camera.updateProjectionMatrix()

    // Studio lighting: hemisphere + strong key + cool rim + warm fill
    // Physical light scale: these values are in candela for point/spot; directional needs higher values
    const hemi = new THREE.HemisphereLight(0xb0c8ee, 0x3d2b1f, 2.5)
    this.scene.add(hemi)

    const keyLight = new THREE.DirectionalLight(0xfff8f0, 5.5)
    keyLight.position.set(0.4, 0.7, 0.3)
    keyLight.castShadow = true
    keyLight.shadow.mapSize.set(2048, 2048)
    keyLight.shadow.camera.near = 0.001
    keyLight.shadow.camera.far = 2
    keyLight.shadow.camera.left = -0.5
    keyLight.shadow.camera.right = 0.5
    keyLight.shadow.camera.top = 0.5
    keyLight.shadow.camera.bottom = -0.5
    keyLight.shadow.bias = -0.0005
    this.scene.add(keyLight)

    // Cool rim/back light for board edge glow
    const rimLight = new THREE.DirectionalLight(0x6080ff, 1.2)
    rimLight.position.set(-0.5, 0.3, -0.5)
    this.scene.add(rimLight)

    // Warm front fill (reduces harsh shadows on component faces)
    const fill = new THREE.DirectionalLight(0xfff4e0, 1.0)
    fill.position.set(0.1, 0.5, 0.6)
    this.scene.add(fill)

    // Under-bounce fill (simulates light bouncing off the desk, warms up board underside)
    const underFill = new THREE.DirectionalLight(0xffeecc, 0.4)
    underFill.position.set(0, -1, 0)
    this.scene.add(underFill)

    // Ground shadow plane (receives shadows only)
    const groundGeo = new THREE.PlaneGeometry(2, 2)
    const groundMat = new THREE.ShadowMaterial({ opacity: 0.35 })
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

    // Capture-harness hooks: ?fastgl=1 skips SSAO (software-GL headless
    // chromium takes seconds per SSAO frame, freezing input/recording);
    // ?orbit=1 turns on a slow cinematic auto-rotate so recordings never
    // need synthetic mouse input at all.
    const params = new URLSearchParams(window.location.search)
    const fastgl = params.get('fastgl') === '1'
    if (params.get('orbit') === '1') {
      this.controls.autoRotate = true
      this.controls.autoRotateSpeed = 1.1
    }

    // Post-processing composer: SSAO for per-component ambient occlusion
    // Modest settings, boards are well under 500k tris so this is cheap
    // on real GPUs.
    this.composer = new EffectComposer(this.renderer)
    const renderPass = new RenderPass(this.scene, this.camera)
    this.composer.addPass(renderPass)

    if (!fastgl) {
      const ssaoPass = new SSAOPass(this.scene, this.camera, rW, rH)
      ssaoPass.kernelRadius = 0.018 // tight radius for mm-scale PCB details
      ssaoPass.minDistance = 0.0005
      ssaoPass.maxDistance = 0.025
      ssaoPass.output = SSAOPass.OUTPUT.Default
      this.composer.addPass(ssaoPass)
    }

    const outputPass = new OutputPass()
    this.composer.addPass(outputPass)

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

    // Point orbit controls at board center
    this.controls.target.copy(center)

    // Frame the board so it fills ~72% of the viewport centered in frame.
    // Key insight: with an oblique camera angle, we need to offset the look-at
    // point slightly toward the camera (upward in world space) so the board
    // appears centered rather than in the lower half.
    const fovRad = (this.camera.fov * Math.PI) / 180
    const boardSpan = Math.max(size.x, size.z) // board footprint span in metres
    const boardDiag = Math.sqrt(size.x * size.x + size.z * size.z)

    // Solve the orbit distance exactly: project the bounding-box corners at
    // each trial distance and binary-search until the larger of the
    // horizontal/vertical screen extents hits the target fill. This accounts
    // for elevation foreshortening and aspect ratio, which closed-form
    // diagonal formulas under-fill badly on flat boards.
    const targetFill = 0.72
    const elevAngle = 0.52 // ~30° product-photo elevation
    const sideOffset = 0.15
    const halfFovV = fovRad / 2
    const aspect = this.camera.aspect
    const halfFovH = Math.atan(Math.tan(halfFovV) * aspect)
    const corners: THREE.Vector3[] = []
    for (const sx of [-0.5, 0.5])
      for (const sy of [-0.5, 0.5])
        for (const sz of [-0.5, 0.5])
          corners.push(new THREE.Vector3(
            center.x + sx * size.x, center.y + sy * size.y, center.z + sz * size.z))
    const fillAt = (d: number): number => {
      const eye = new THREE.Vector3(
        center.x + sideOffset * boardSpan,
        center.y + d * Math.sin(elevAngle),
        center.z + d * Math.cos(elevAngle))
      const m = new THREE.Matrix4().lookAt(eye, center, new THREE.Vector3(0, 1, 0))
      const inv = m.clone().invert()
      let maxH = 0, maxV = 0
      for (const c of corners) {
        const p = c.clone().sub(eye).applyMatrix4(inv)
        const depth = -p.z
        if (depth <= 1e-9) return 10 // inside/behind: way overfilled
        maxH = Math.max(maxH, Math.abs(p.x) / (depth * Math.tan(halfFovH)))
        maxV = Math.max(maxV, Math.abs(p.y) / (depth * Math.tan(halfFovV)))
      }
      return Math.max(maxH, maxV)
    }
    let lo = boardDiag * 0.05, hi = boardDiag * 20
    for (let i = 0; i < 40; i++) {
      const mid = (lo + hi) / 2
      if (fillAt(mid) > targetFill) lo = mid
      else hi = mid
    }
    const dist = (lo + hi) / 2
    this.camera.position.set(
      center.x + sideOffset * boardSpan,
      center.y + dist * Math.sin(elevAngle),
      center.z + dist * Math.cos(elevAngle),
    )

    // Shift the lookAt point slightly toward the board center (downward in world space) to
    // compensate for the overhead camera angle so the board stays visually centered.
    // The shift is proportional to the board's vertical extent to stay board-size-agnostic.
    const lookAtShift = new THREE.Vector3(0, -dist * 0.06, 0)
    this.controls.target.copy(center).add(lookAtShift)
    this.camera.near = maxDim * 0.001
    this.camera.far = maxDim * 30
    this.camera.updateProjectionMatrix()
    this.controls.minDistance = dist * 0.2
    this.controls.maxDistance = dist * 4

    // Force OrbitControls damping to converge immediately so the camera is
    // actually at the solved position when the screenshot fires (not mid-damp).
    for (let i = 0; i < 60; i++) this.controls.update()

    // Scale shadow camera to board size for correct shadow coverage
    const shadowHalf = boardSpan * 0.75
    const keyLightObj = this.scene.children.find(c => c instanceof THREE.DirectionalLight && (c as THREE.DirectionalLight).intensity > 4) as THREE.DirectionalLight | undefined
    if (keyLightObj) {
      keyLightObj.position.set(center.x + boardSpan * 0.8, boardSpan * 1.2, center.z + boardSpan * 0.6)
      keyLightObj.target.position.copy(center)
      keyLightObj.shadow.camera.left = -shadowHalf
      keyLightObj.shadow.camera.right = shadowHalf
      keyLightObj.shadow.camera.top = shadowHalf
      keyLightObj.shadow.camera.bottom = -shadowHalf
      keyLightObj.shadow.camera.far = boardSpan * 6
      keyLightObj.shadow.camera.updateProjectionMatrix()
      this.scene.add(keyLightObj.target)
    }

    // Enable shadows on all meshes; apply material upgrades
    // Pass 1: measure mesh areas to identify largest-area meshes (candidate substrate)
    const meshAreas: { mesh: THREE.Mesh; approxArea: number }[] = []
    model.traverse(obj => {
      const mesh = obj as THREE.Mesh
      if (!mesh.isMesh) return
      const geo = mesh.geometry as THREE.BufferGeometry
      const pos = geo.attributes.position
      if (pos) {
        // Approximate area via bounding box volume, large flat board has large XZ area
        geo.computeBoundingBox()
        const bb = geo.boundingBox!
        const sz = new THREE.Vector3()
        bb.getSize(sz)
        meshAreas.push({ mesh, approxArea: sz.x * sz.z })
      }
    })
    const sortedByArea = [...meshAreas].sort((a, b) => b.approxArea - a.approxArea)
    // Largest-area meshes (top 3) are likely the board substrate and copper layers
    const substrateCandidates = new Set(sortedByArea.slice(0, 3).map(e => e.mesh))

    model.traverse(obj => {
      const mesh = obj as THREE.Mesh
      if (!mesh.isMesh) return
      mesh.castShadow = true
      mesh.receiveShadow = true

      const mats = Array.isArray(mesh.material) ? mesh.material : [mesh.material]
      const newMats: THREE.Material[] = []
      const isSubstrate = substrateCandidates.has(mesh)

      for (const mat of mats) {
        const hasPBR = mat instanceof THREE.MeshStandardMaterial || mat instanceof THREE.MeshPhysicalMaterial
        const hasFlatColor = mat instanceof THREE.MeshBasicMaterial || mat instanceof THREE.MeshLambertMaterial || mat instanceof THREE.MeshPhongMaterial
        const matWithColor = mat as THREE.MeshStandardMaterial

        if (!hasPBR && !hasFlatColor) {
          newMats.push(mat)
          continue
        }

        const c = matWithColor.color
        const lr = c.r, lg = c.g, lb = c.b
        const luminance = 0.2126 * lr + 0.7152 * lg + 0.0722 * lb
        const maxC = Math.max(lr, lg, lb)
        const minC = Math.min(lr, lg, lb)
        const saturation = maxC > 0.01 ? (maxC - minC) / maxC : 0

        // Detect green-family solder mask: hue in green range (G is dominant channel),
        // or detect the mint-green / forest-green range by checking G > R and G > B.
        // Also handles the off-white KiCad placeholder green.
        const isGreenFamily = lg > lr * 1.05 && lg > lb * 1.05
        // Detect cream/ivory board (stickhub): warm, mid-high luminance, low saturation
        const isCreamFamily = luminance > 0.4 && luminance < 0.85 && saturation < 0.45 && lr >= lg && lg >= lb

        if (isGreenFamily || (isSubstrate && isCreamFamily)) {
          // Upgrade to MeshPhysicalMaterial with clearcoat for semi-gloss soldermask look
          const oldMat = mat as THREE.MeshStandardMaterial | THREE.MeshPhysicalMaterial

          const physical = new THREE.MeshPhysicalMaterial({
            roughness: 0.35,
            metalness: oldMat instanceof THREE.MeshStandardMaterial ? oldMat.metalness : 0,
            clearcoat: 0.6,
            clearcoatRoughness: 0.25,
            envMapIntensity: 1.2,
            side: oldMat.side,
            map: oldMat instanceof THREE.MeshStandardMaterial ? (oldMat.map ?? null) : null,
          })

          if (isGreenFamily) {
            // Saturated PCB green, #0d5c2e in linear sRGB
            physical.color.set(0x0d5c2e)
          } else {
            // Cream board (stickhub): deepen and saturate the original color, keep identity
            // Shift toward a richer warm off-white, darken ~25% and bump saturation
            physical.color.setRGB(lr * 0.75, lg * 0.68, lb * 0.60)
          }

          newMats.push(physical)
          continue
        }

        // Non-substrate: high-luminance low-saturation flat materials (cream placeholders)
        if (hasPBR && luminance > 0.55 && saturation < 0.45 && (mat as THREE.MeshStandardMaterial).roughness >= 0.8) {
          const pbr = mat as THREE.MeshStandardMaterial
          pbr.color.multiplyScalar(0.5)
          pbr.roughness = 0.45
          pbr.metalness = 0.0
          pbr.envMapIntensity = 2.0
          pbr.needsUpdate = true
          newMats.push(mat)
          continue
        }

        if (hasPBR) {
          // General PBR materials: dial up specular slightly
          const pbr = mat as THREE.MeshStandardMaterial
          if (pbr.roughness > 0.7) pbr.roughness = Math.max(0.45, pbr.roughness - 0.2)
          pbr.envMapIntensity = 1.5
          pbr.needsUpdate = true
        }
        newMats.push(mat)
      }

      if (Array.isArray(mesh.material)) {
        mesh.material = newMats
      } else if (newMats.length === 1) {
        mesh.material = newMats[0]
      }
    })

    // Remove any previous board model (keep lights, ground, markers)
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
    faults?: { component: string; kind: string; value: number; limit: number; t: number }[],
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
    this.composer.setSize(w, h)
  }

  private startLoop() {
    const tick = () => {
      if (this.disposed) return
      this.animHandle = requestAnimationFrame(tick)
      this.controls.update()
      // Render via composer so SSAO pass fires each frame
      this.composer.render()
    }
    this.animHandle = requestAnimationFrame(tick)
  }

  dispose() {
    this.disposed = true
    if (this.animHandle !== null) cancelAnimationFrame(this.animHandle)
    this.controls.dispose()
    this.composer.dispose()
    this.renderer.dispose()
  }
}
